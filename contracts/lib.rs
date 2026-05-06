#![cfg_attr(not(feature = "std"), no_std, no_main)]

#[ink::contract]
mod wallet_guardian {
    use ink::prelude::string::String;

    // ──────────────────────────────────────────────
    // DOMAIN TYPES
    // ──────────────────────────────────────────────

    /// Price is expressed as u128 with 6 decimal precision.
    /// Example: $1.25 DOT = 1_250_000
    pub type Price = u128;

    #[derive(Debug, Clone, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(
        feature = "std",
        derive(scale_info::TypeInfo, ink::storage::traits::StorageLayout)
    )]
    pub struct Rule {
        /// Price level that triggers this rule (6dp scaled u128)
        pub threshold_price: Price,
        /// true  → trigger when price DROPS BELOW threshold
        /// false → trigger when price RISES ABOVE threshold
        pub is_below: bool,
        /// Destination wallet when rule fires
        pub target_account: AccountId,
        /// Amount (in planck / smallest unit) to transfer on trigger
        pub amount: Balance,
        /// Rule becomes inactive after first execution
        pub is_active: bool,
    }

    // ──────────────────────────────────────────────
    // ERRORS
    // ──────────────────────────────────────────────

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum WalletGuardianError {
        /// Caller is not the contract owner
        Unauthorized,
        /// No rule has been configured
        NoRuleConfigured,
        /// Rule exists but has already fired
        RuleAlreadyExecuted,
        /// Price condition has not been met
        ConditionNotMet,
        /// Contract does not hold enough balance
        InsufficientContractBalance,
        /// Native transfer to target account failed
        TransferFailed,
        /// Attempt to set zero-amount rule
        InvalidRuleAmount,
        /// Threshold price cannot be zero
        InvalidThresholdPrice,
    }

    pub type Result<T> = core::result::Result<T, WalletGuardianError>;

    // ──────────────────────────────────────────────
    // EVENTS
    // ──────────────────────────────────────────────

    /// Emitted when the owner deposits native tokens
    #[ink(event)]
    pub struct DepositReceived {
        #[ink(topic)]
        pub from: AccountId,
        pub amount: Balance,
        pub new_balance: Balance,
    }

    /// Emitted when the owner sets or replaces the rule
    #[ink(event)]
    pub struct RuleSet {
        #[ink(topic)]
        pub owner: AccountId,
        pub threshold_price: Price,
        pub is_below: bool,
        pub target_account: AccountId,
        pub amount: Balance,
    }

    /// Emitted when the watcher successfully triggers execution
    #[ink(event)]
    pub struct RuleExecuted {
        #[ink(topic)]
        pub triggered_by: AccountId,
        pub price_at_execution: Price,
        pub target_account: AccountId,
        pub amount_transferred: Balance,
    }

    /// Emitted when owner withdraws remaining balance
    #[ink(event)]
    pub struct OwnerWithdrawal {
        #[ink(topic)]
        pub owner: AccountId,
        pub amount: Balance,
    }

    // ──────────────────────────────────────────────
    // STORAGE
    // ──────────────────────────────────────────────

    #[ink(storage)]
    pub struct WalletGuardian {
        /// Contract deployer — only address that can set rules / withdraw
        owner: AccountId,
        /// Tracked native balance (informational; real balance via self_balance())
        tracked_balance: Balance,
        /// The single active rule (None until set_rule is called)
        rule: Option<Rule>,
    }

    // ──────────────────────────────────────────────
    // IMPLEMENTATION
    // ──────────────────────────────────────────────

    impl WalletGuardian {
        // ── Constructor ──────────────────────────

        /// Deploys the contract. Caller becomes the permanent owner.
        /// Optionally accepts an initial deposit at construction time.
        #[ink(constructor, payable)]
        pub fn new() -> Self {
            let caller = Self::env().caller();
            let initial = Self::env().transferred_value();

            if initial > 0 {
                Self::env().emit_event(DepositReceived {
                    from: caller,
                    amount: initial,
                    new_balance: initial,
                });
            }

            Self {
                owner: caller,
                tracked_balance: initial,
                rule: None,
            }
        }

        // ── Write: Deposit ────────────────────────

        /// Accepts native token deposits from anyone.
        /// Practically only the owner would fund their own guardian wallet.
        #[ink(message, payable)]
        pub fn deposit(&mut self) {
            let amount = self.env().transferred_value();
            self.tracked_balance = self.tracked_balance.saturating_add(amount);

            self.env().emit_event(DepositReceived {
                from: self.env().caller(),
                amount,
                new_balance: self.env().balance(),
            });
        }

        // ── Write: Set Rule ───────────────────────

        /// Owner sets (or replaces) the single automation rule.
        ///
        /// # Arguments
        /// * `threshold_price` — 6dp-scaled price (e.g. 4_500_000 = $4.50)
        /// * `is_below`        — true = fire when price < threshold
        /// * `target_account`  — recipient of the transfer
        /// * `amount`          — planck amount to send
        #[ink(message)]
        pub fn set_rule(
            &mut self,
            threshold_price: Price,
            is_below: bool,
            target_account: AccountId,
            amount: Balance,
        ) -> Result<()> {
            self.require_owner()?;

            if amount == 0 {
                return Err(WalletGuardianError::InvalidRuleAmount);
            }
            if threshold_price == 0 {
                return Err(WalletGuardianError::InvalidThresholdPrice);
            }

            self.rule = Some(Rule {
                threshold_price,
                is_below,
                target_account,
                amount,
                is_active: true,
            });

            self.env().emit_event(RuleSet {
                owner: self.owner,
                threshold_price,
                is_below,
                target_account,
                amount,
            });

            Ok(())
        }

        // ── Write: Execute Rule ───────────────────

        /// Called by the off-chain watcher when it believes the condition is met.
        ///
        /// The contract re-validates the condition independently — the watcher
        /// is untrusted. Anyone can call this; the contract is the source of truth.
        ///
        /// # Arguments
        /// * `current_price` — 6dp-scaled price the watcher observed
        #[ink(message)]
        pub fn execute_rule(&mut self, current_price: Price) -> Result<()> {
            let rule = self
                .rule
                .as_ref()
                .ok_or(WalletGuardianError::NoRuleConfigured)?
                .clone();

            if !rule.is_active {
                return Err(WalletGuardianError::RuleAlreadyExecuted);
            }

            // ── Condition guard ──
            let condition_met = if rule.is_below {
                current_price < rule.threshold_price
            } else {
                current_price > rule.threshold_price
            };

            if !condition_met {
                return Err(WalletGuardianError::ConditionNotMet);
            }

            // ── Balance guard ──
            let contract_balance = self.env().balance();
            if contract_balance < rule.amount {
                return Err(WalletGuardianError::InsufficientContractBalance);
            }

            // ── Deactivate before transfer (reentrancy pattern) ──
            if let Some(ref mut r) = self.rule {
                r.is_active = false;
            }
            self.tracked_balance = self.tracked_balance.saturating_sub(rule.amount);

            // ── Transfer ──
            self.env()
                .transfer(rule.target_account, rule.amount)
                .map_err(|_| WalletGuardianError::TransferFailed)?;

            self.env().emit_event(RuleExecuted {
                triggered_by: self.env().caller(),
                price_at_execution: current_price,
                target_account: rule.target_account,
                amount_transferred: rule.amount,
            });

            Ok(())
        }

        // ── Write: Owner Withdraw ─────────────────

        /// Owner reclaims all remaining contract balance.
        /// Also clears any pending rule.
        #[ink(message)]
        pub fn withdraw(&mut self) -> Result<()> {
            self.require_owner()?;

            let balance = self.env().balance();
            if balance == 0 {
                return Ok(());
            }

            // Clear rule on full withdrawal — prevents ghost rules
            self.rule = None;
            self.tracked_balance = 0;

            self.env()
                .transfer(self.owner, balance)
                .map_err(|_| WalletGuardianError::TransferFailed)?;

            self.env().emit_event(OwnerWithdrawal {
                owner: self.owner,
                amount: balance,
            });

            Ok(())
        }

        // ── Read: Get Rule ────────────────────────

        #[ink(message)]
        pub fn get_rule(&self) -> Option<Rule> {
            self.rule.clone()
        }

        /// Live contract balance (planck)
        #[ink(message)]
        pub fn get_balance(&self) -> Balance {
            self.env().balance()
        }

        /// Contract owner address
        #[ink(message)]
        pub fn get_owner(&self) -> AccountId {
            self.owner
        }

        /// Convenience: would execute_rule succeed at this price right now?
        #[ink(message)]
        pub fn check_condition(&self, current_price: Price) -> bool {
            match &self.rule {
                None => false,
                Some(rule) if !rule.is_active => false,
                Some(rule) => {
                    if rule.is_below {
                        current_price < rule.threshold_price
                    } else {
                        current_price > rule.threshold_price
                    }
                }
            }
        }

        // ── Private Helpers ───────────────────────

        fn require_owner(&self) -> Result<()> {
            if self.env().caller() != self.owner {
                return Err(WalletGuardianError::Unauthorized);
            }
            Ok(())
        }
    }

    // ──────────────────────────────────────────────
    // TESTS
    // ──────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use ink::env::test;

        fn default_accounts() -> test::DefaultAccounts<ink::env::DefaultEnvironment> {
            test::default_accounts::<ink::env::DefaultEnvironment>()
        }

        fn set_caller(account: AccountId) {
            test::set_caller::<ink::env::DefaultEnvironment>(account);
        }

        fn set_balance(account: AccountId, balance: Balance) {
            test::set_account_balance::<ink::env::DefaultEnvironment>(account, balance);
        }

        #[ink::test]
        fn deploy_sets_owner() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            let contract = WalletGuardian::new();
            assert_eq!(contract.get_owner(), accounts.alice);
        }

        #[ink::test]
        fn set_rule_requires_owner() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            let mut contract = WalletGuardian::new();

            set_caller(accounts.bob);
            let result = contract.set_rule(4_000_000, true, accounts.charlie, 100);
            assert_eq!(result, Err(WalletGuardianError::Unauthorized));
        }

        #[ink::test]
        fn set_rule_stores_correctly() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            let mut contract = WalletGuardian::new();

            contract
                .set_rule(4_000_000, true, accounts.bob, 500_000)
                .unwrap();

            let rule = contract.get_rule().unwrap();
            assert_eq!(rule.threshold_price, 4_000_000);
            assert!(rule.is_below);
            assert!(rule.is_active);
            assert_eq!(rule.amount, 500_000);
        }

        #[ink::test]
        fn check_condition_returns_correct_state() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            let mut contract = WalletGuardian::new();

            // Rule: fire if price < 4.00 (4_000_000)
            contract
                .set_rule(4_000_000, true, accounts.bob, 100)
                .unwrap();

            assert!(contract.check_condition(3_500_000)); // below threshold → true
            assert!(!contract.check_condition(4_500_000)); // above threshold → false
            assert!(!contract.check_condition(4_000_000)); // equal → false (strict less-than)
        }

        #[ink::test]
        fn execute_rule_transfers_and_deactivates() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            set_balance(accounts.alice, 10_000_000);

            let mut contract = WalletGuardian::new();
            test::set_value_transferred::<ink::env::DefaultEnvironment>(5_000_000);
            contract.deposit();

            contract
                .set_rule(4_000_000, true, accounts.bob, 1_000_000)
                .unwrap();

            // Trigger at price 3_500_000 (below 4_000_000 threshold)
            contract.execute_rule(3_500_000).unwrap();

            // Rule must be deactivated after execution
            let rule = contract.get_rule().unwrap();
            assert!(!rule.is_active);
        }

        #[ink::test]
        fn execute_rule_rejects_unmet_condition() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            let mut contract = WalletGuardian::new();

            contract
                .set_rule(4_000_000, true, accounts.bob, 100)
                .unwrap();

            // Price is above threshold; condition not met
            let result = contract.execute_rule(5_000_000);
            assert_eq!(result, Err(WalletGuardianError::ConditionNotMet));
        }

        #[ink::test]
        fn execute_rule_blocks_double_execution() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            set_balance(accounts.alice, 10_000_000);
            let mut contract = WalletGuardian::new();

            test::set_value_transferred::<ink::env::DefaultEnvironment>(5_000_000);
            contract.deposit();
            contract
                .set_rule(4_000_000, true, accounts.bob, 100)
                .unwrap();

            contract.execute_rule(3_000_000).unwrap();

            // Second call must fail — rule is now inactive
            let result = contract.execute_rule(3_000_000);
            assert_eq!(result, Err(WalletGuardianError::RuleAlreadyExecuted));
        }

        #[ink::test]
        fn invalid_rule_params_rejected() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            let mut contract = WalletGuardian::new();

            // Zero amount
            assert_eq!(
                contract.set_rule(4_000_000, true, accounts.bob, 0),
                Err(WalletGuardianError::InvalidRuleAmount)
            );

            // Zero threshold
            assert_eq!(
                contract.set_rule(0, true, accounts.bob, 100),
                Err(WalletGuardianError::InvalidThresholdPrice)
            );
        }
    }
}