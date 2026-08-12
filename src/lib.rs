pub mod error;
pub mod quote;
pub mod state;

use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use solana_program::instruction::AccountMeta;
use solana_program::pubkey::Pubkey;

use dflow_amm_interface::{
    AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, Quote, QuoteParams, Swap,
    SwapAndAccountMetas, SwapParams,
};

use crate::quote::{compute_quote, QuoteOutput};
use crate::state::{
    deserialize_maker_book, deserialize_market_header, deserialize_registry, MakerBook,
    MarketStateHeader, MARKET_DISCRIMINATOR,
};

pub const ARCHER_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("Archer8kgiavM61GyusMzaaS2ft5sALtNsD1HxkUPMhy");

const SWAP_DISCRIMINATOR: u8 = 15;

const SPL_TOKEN_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

const TOKEN_2022_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

#[derive(Clone)]
pub struct ArcherAmm {
    pub market_key: Pubkey,
    pub registry_key: Pubkey,

    pub market_header: Option<MarketStateHeader>,
    pub maker_book_keys: Vec<Pubkey>,
    pub maker_books: Vec<(Pubkey, MakerBook)>,

    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,

    base_mint_data: Vec<u8>,
    quote_mint_data: Vec<u8>,

    pub clock_ref: ClockRef,

    /// Quote token account that receives the integrator's share of taker fees.
    pub integrator_fee_wallet: Pubkey,
}

impl ArcherAmm {
    fn current_slot(&self) -> u64 {
        self.clock_ref.slot.load(Ordering::Relaxed)
    }

    fn current_epoch(&self) -> u64 {
        self.clock_ref.epoch.load(Ordering::Relaxed)
    }
}

impl Amm for ArcherAmm {
    fn from_keyed_account(
        keyed_account: &KeyedAccount,
        amm_context: &AmmContext,
    ) -> Result<Self> {
        let market_key = keyed_account.key;
        let data = &keyed_account.account.data;

        if data.len() < 8 || &data[0..8] != MARKET_DISCRIMINATOR {
            return Err(anyhow!("Not an Archer market"));
        }

        let (registry_key, _) = Pubkey::find_program_address(
            &[b"maker_registry", market_key.as_ref()],
            &ARCHER_PROGRAM_ID,
        );

        Ok(Self {
            market_key,
            registry_key,
            market_header: None,
            maker_book_keys: vec![],
            maker_books: vec![],
            base_token_program: SPL_TOKEN_PROGRAM,
            quote_token_program: SPL_TOKEN_PROGRAM,
            base_mint_data: vec![],
            quote_mint_data: vec![],
            clock_ref: amm_context.clock_ref.clone(),
            integrator_fee_wallet: Pubkey::default(),
        })
    }

    fn label(&self) -> String {
        "Archer".to_string()
    }

    fn program_id(&self) -> Pubkey {
        ARCHER_PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.market_key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        match &self.market_header {
            Some(h) => vec![h.base_mint, h.quote_mint],
            None => vec![],
        }
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut accounts = vec![self.market_key, self.registry_key];
        accounts.extend_from_slice(&self.maker_book_keys);
        if let Some(h) = &self.market_header {
            accounts.push(h.base_mint);
            accounts.push(h.quote_mint);
        }
        accounts
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        if let Some(market_account) = account_map.get(&self.market_key) {
            let header = deserialize_market_header(&market_account.data)
                .map_err(|e| anyhow!("Failed to deserialize market: {e}"))?;
            self.market_header = Some(header);
        }

        if let Some(header) = &self.market_header {
            if let Some(base_mint_account) = account_map.get(&header.base_mint) {
                self.base_token_program =
                    detect_token_program_from_data(&base_mint_account.data);
                self.base_mint_data = base_mint_account.data.clone();
            }
            if let Some(quote_mint_account) = account_map.get(&header.quote_mint) {
                self.quote_token_program =
                    detect_token_program_from_data(&quote_mint_account.data);
                self.quote_mint_data = quote_mint_account.data.clone();
            }
        }

        if let Some(registry_account) = account_map.get(&self.registry_key) {
            let data = &registry_account.data;
            if data.len() >= state::MakerRegistry::LEN
                && &data[0..8] == state::REGISTRY_DISCRIMINATOR
            {
                let registry = deserialize_registry(data)
                    .map_err(|e| anyhow!("Failed to deserialize registry: {e}"))?;
                let num = registry.num_makers as usize;
                let mut deduped: Vec<Pubkey> = Vec::with_capacity(num);
                for key in &registry.makers[..num] {
                    if !deduped.contains(key) {
                        deduped.push(*key);
                    }
                }
                self.maker_book_keys = deduped;
            }
        }

        self.maker_books.clear();
        for book_key in &self.maker_book_keys {
            if let Some(book_account) = account_map.get(book_key) {
                if let Ok(book) = deserialize_maker_book(&book_account.data) {
                    self.maker_books.push((*book_key, book));
                }
            }
        }

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let header = self
            .market_header
            .as_ref()
            .ok_or_else(|| anyhow!("Market not loaded"))?;

        if !header.is_active() {
            return Err(anyhow!("Market not active"));
        }

        let is_buy = quote_params.input_mint == header.quote_mint;
        let current_slot = self.current_slot();
        let current_epoch = self.current_epoch();

        let (input_mint_data, input_token_program, output_mint_data, output_token_program) =
            if is_buy {
                (
                    &self.quote_mint_data,
                    &self.quote_token_program,
                    &self.base_mint_data,
                    &self.base_token_program,
                )
            } else {
                (
                    &self.base_mint_data,
                    &self.base_token_program,
                    &self.quote_mint_data,
                    &self.quote_token_program,
                )
            };

        let input_transfer_fee = transfer_fee_atoms(
            input_mint_data,
            input_token_program,
            quote_params.amount,
            current_epoch,
        )
        .map_err(|e| anyhow!("{e}"))?;
        let net_input = quote_params.amount.saturating_sub(input_transfer_fee);

        let QuoteOutput {
            out_amount,
            fee_amount: _,
        } = compute_quote(
            net_input,
            is_buy,
            header,
            &self.maker_books,
            current_slot,
            None,
        )
        .map_err(|e| anyhow!("{e}"))?;

        let output_transfer_fee = transfer_fee_atoms(
            output_mint_data,
            output_token_program,
            out_amount,
            current_epoch,
        )
        .map_err(|e| anyhow!("{e}"))?;
        let net_output = out_amount.saturating_sub(output_transfer_fee);

        Ok(Quote {
            in_amount: quote_params.amount,
            out_amount: net_output,
        })
    }

    fn get_swap_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<SwapAndAccountMetas> {
        let header = self
            .market_header
            .as_ref()
            .ok_or_else(|| anyhow!("Market not loaded"))?;

        let is_buy = swap_params.source_mint == header.quote_mint;
        let side: u8 = if is_buy { 0 } else { 1 }; // Bid=0, Ask=1

        let (taker_base_ata, taker_quote_ata) = if is_buy {
            (
                swap_params.destination_token_account,
                swap_params.source_token_account,
            )
        } else {
            (
                swap_params.source_token_account,
                swap_params.destination_token_account,
            )
        };

        let mut account_metas = vec![
            AccountMeta::new_readonly(swap_params.token_transfer_authority, true),
            AccountMeta::new(self.market_key, false),
            AccountMeta::new(self.integrator_fee_wallet, false),
            AccountMeta::new_readonly(header.base_mint, false),
            AccountMeta::new_readonly(header.quote_mint, false),
            AccountMeta::new(header.base_vault, false),
            AccountMeta::new(header.quote_vault, false),
            AccountMeta::new(taker_base_ata, false),
            AccountMeta::new(taker_quote_ata, false),
            AccountMeta::new_readonly(self.base_token_program, false),
            AccountMeta::new_readonly(self.quote_token_program, false),
        ];

        let current_slot = self.current_slot();
        for (book_key, book) in &self.maker_books {
            if book.is_active() && !book.is_stale(current_slot) {
                account_metas.push(AccountMeta::new(*book_key, false));
            }
        }

        let input_lots = if is_buy {
            swap_params
                .in_amount
                .checked_div(header.quote_atoms_per_quote_lot)
                .ok_or_else(|| anyhow!("quote lot size is 0"))?
        } else {
            swap_params
                .in_amount
                .checked_div(header.base_atoms_per_base_lot)
                .ok_or_else(|| anyhow!("base lot size is 0"))?
        };

        let mut data = Vec::with_capacity(19);
        data.push(SWAP_DISCRIMINATOR);
        data.extend_from_slice(&input_lots.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.push(side);
        data.push(0u8);

        Ok(SwapAndAccountMetas {
            swap: Swap::Archer,
            account_metas,
        })
    }

    fn has_dynamic_accounts(&self) -> bool {
        true // maker book keys are discovered from the registry during update()
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        true // need market header to know the mints
    }

    fn supports_exact_out(&self) -> bool {
        false
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    fn is_active(&self) -> bool {
        self.market_header
            .as_ref()
            .map(|h| h.is_active())
            .unwrap_or(false)
    }

    fn get_accounts_len(&self) -> usize {
        // 11 fixed accounts + maker books
        11 + self.maker_books.len()
    }
}

fn detect_token_program_from_data(mint_data: &[u8]) -> Pubkey {
    if mint_data.len() > 82 {
        TOKEN_2022_PROGRAM
    } else {
        SPL_TOKEN_PROGRAM
    }
}

fn transfer_fee_atoms(
    mint_data: &[u8],
    token_program: &Pubkey,
    amount: u64,
    epoch: u64,
) -> Result<u64> {
    if *token_program != TOKEN_2022_PROGRAM || mint_data.is_empty() {
        return Ok(0);
    }

    use spl_token_2022::extension::{
        transfer_fee::TransferFeeConfig, BaseStateWithExtensions, StateWithExtensions,
    };
    use spl_token_2022::state::Mint;

    let state = StateWithExtensions::<Mint>::unpack(mint_data)
        .map_err(|e| anyhow!("mint unpack: {e}"))?;

    let cfg = match state.get_extension::<TransferFeeConfig>() {
        Ok(cfg) => cfg,
        Err(_) => return Ok(0),
    };

    let fee = cfg
        .calculate_epoch_fee(epoch, amount)
        .ok_or_else(|| anyhow!("transfer fee overflow"))?;

    Ok(fee)
}
