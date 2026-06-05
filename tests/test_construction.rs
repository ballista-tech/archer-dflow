#[cfg(test)]
mod test_construction {
    //! Integration test ensuring that the Archer AMM:
    //! - can be constructed from a KeyedAccount,
    //! - can load its required state via the Amm trait lifecycle,
    //! - returns valid reserve mints,
    //! - supports quoting for both swap directions,
    //! - and exposes sane quoting boundaries.

    use std::env;
    use std::str::FromStr;

    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_sdk::pubkey::Pubkey;

    use dflow_amm_interface::{
        AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode,
    };

    use archer_dflow::ArcherAmm;

    fn init_test_logger() {
        let _ = dotenvy::dotenv();
        let _ = env_logger::builder().is_test(true).try_init();
    }

    /// Fetch accounts from RPC and build an AccountMap for the Amm trait.
    async fn fetch_account_map(rpc: &RpcClient, keys: &[Pubkey]) -> AccountMap {
        let mut map = AccountMap::default();
        for key in keys {
            if let Ok(account) = rpc.get_account(key).await {
                map.insert(*key, account);
            }
        }
        map
    }

    /// Compute quoting bounds for a given input mint direction.
    fn compute_bounds(amm: &ArcherAmm, input_mint: &Pubkey, output_mint: &Pubkey) -> (u64, u64) {
        let mints = amm.get_reserve_mints();
        let is_buy = *input_mint == mints[1]; // quote mint is second

        let header = amm.market_header.as_ref().unwrap();
        let min_lot = if is_buy {
            header.quote_atoms_per_quote_lot
        } else {
            header.base_atoms_per_base_lot
        };

        let quote_output = |amount: u64| -> u64 {
            amm.quote(&QuoteParams {
                amount,
                input_mint: *input_mint,
                output_mint: *output_mint,
                swap_mode: SwapMode::ExactIn,
            })
            .map(|r| r.out_amount)
            .unwrap_or(0)
        };

        let max_output = quote_output(u64::MAX / 2);
        if max_output == 0 {
            return (min_lot, min_lot);
        }

        // Binary search for smallest input producing non-zero output
        let mut lo = min_lot;
        let mut hi = u64::MAX / 2;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if quote_output(mid) > 0 {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let lower_bound = lo;

        // Binary search for smallest input achieving max output (saturation)
        lo = lower_bound;
        hi = u64::MAX / 2;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if quote_output(mid) >= max_output {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let upper_bound = lo;

        (lower_bound, upper_bound)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_construction() {
        init_test_logger();

        let market_key_str = env::var("ARCHER_MARKET_KEY")
            .expect("ARCHER_MARKET_KEY must be set for integration tests");
        let market_key = Pubkey::from_str(&market_key_str).expect("Invalid market pubkey");

        let rpc_url =
            env::var("SOLANA_RPC_URL").expect("SOLANA_RPC_URL must be set for integration tests");
        let rpc = RpcClient::new(rpc_url);

        // Step 1: Fetch market account and construct via from_keyed_account
        let market_account = rpc
            .get_account(&market_key)
            .await
            .expect("Failed to fetch market account");

        let keyed_account = KeyedAccount {
            key: market_key,
            account: market_account,
            params: None,
        };

        let amm_context = AmmContext {
            clock_ref: ClockRef::default(),
        };

        let mut amm = ArcherAmm::from_keyed_account(&keyed_account, &amm_context)
            .expect("Failed to construct AMM");

        // Step 2: First update — fetches market + registry
        let accounts_to_update = amm.get_accounts_to_update();
        log::info!("First fetch: {} accounts", accounts_to_update.len());

        let account_map = fetch_account_map(&rpc, &accounts_to_update).await;
        amm.update(&account_map)
            .expect("First update failed");

        assert!(
            amm.has_dynamic_accounts(),
            "Archer AMM should have dynamic accounts"
        );

        // Step 3: Second update — now includes maker books discovered from registry
        let accounts_to_update = amm.get_accounts_to_update();
        log::info!("Second fetch: {} accounts", accounts_to_update.len());

        let account_map = fetch_account_map(&rpc, &accounts_to_update).await;
        amm.update(&account_map)
            .expect("Second update failed");

        // Step 4: Validate reserve mints
        assert!(
            amm.requires_update_for_reserve_mints(),
            "Should require update for reserve mints"
        );

        let mints = amm.get_reserve_mints();
        assert_eq!(mints.len(), 2, "Should have exactly 2 reserve mints");
        log::info!("Base mint: {}", mints[0]);
        log::info!("Quote mint: {}", mints[1]);

        assert!(amm.is_active(), "Market should be active");

        // Step 5: Validate quoting in both directions
        let token_pairs = [
            (mints[1], mints[0]), // buy: quote -> base
            (mints[0], mints[1]), // sell: base -> quote
        ];

        for (input_mint, output_mint) in &token_pairs {
            log::info!("Checking bounds for input mint: {}", input_mint);

            let (lower_bound, upper_bound) = compute_bounds(&amm, input_mint, output_mint);

            assert!(
                lower_bound <= upper_bound,
                "Lower bound must be <= upper bound"
            );

            let lb_result = amm
                .quote(&QuoteParams {
                    amount: lower_bound,
                    input_mint: *input_mint,
                    output_mint: *output_mint,
                    swap_mode: SwapMode::ExactIn,
                })
                .expect("Lower-bound quote failed");

            log::info!("Lower-bound quote: {:?}", lb_result);
            assert!(lb_result.out_amount > 0, "Lower bound produced zero output");

            let ub_result = amm
                .quote(&QuoteParams {
                    amount: upper_bound,
                    input_mint: *input_mint,
                    output_mint: *output_mint,
                    swap_mode: SwapMode::ExactIn,
                })
                .expect("Upper-bound quote failed");

            log::info!("Upper-bound quote: {:?}", ub_result);
            assert!(ub_result.out_amount > 0, "Upper bound produced zero output");
        }
    }
}
