"""
Wallet Guardian — Watcher Configuration
All environment-sourced. No hardcoded secrets. Ever.
"""

import os
from dataclasses import dataclass
from typing import Final


def _require_env(key: str) -> str:
    value = os.environ.get(key)
    if not value:
        raise EnvironmentError(
            f"[config] Required environment variable '{key}' is not set. "
            f"Check your .env file or shell environment."
        )
    return value


@dataclass(frozen=True)
class WatcherConfig:
    # ── Substrate / Portaldot RPC ──────────────────────────────────────────
    rpc_url: str
    contract_address: str

    # ── Signer ────────────────────────────────────────────────────────────
    # Mnemonic of the account that will call execute_rule.
    # Does NOT need to be the contract owner — anyone can trigger execution.
    signer_mnemonic: str

    # ── Price Feed ────────────────────────────────────────────────────────
    # CoinGecko coin id, e.g. "polkadot" or "dot"
    coingecko_coin_id: str

    # Currency to quote against — "usd" recommended
    coingecko_vs_currency: str

    # ── Rule Parameters (mirrors on-chain state; used for local pre-check) ─
    # Threshold price in 6dp-scaled integer (e.g. $4.50 → 4_500_000)
    threshold_price: int

    # True  → trigger when price DROPS BELOW threshold
    # False → trigger when price RISES ABOVE threshold
    is_below: bool

    # ── Watcher Behaviour ─────────────────────────────────────────────────
    # Seconds between price polls
    poll_interval_seconds: int

    # Maximum consecutive RPC/API failures before watcher exits with error
    max_consecutive_failures: int

    # Gas limit for execute_rule call (planck)
    gas_limit: int

    # Storage deposit limit (set to None to let runtime estimate)
    storage_deposit_limit: int | None

    # ── Demo / Testing ────────────────────────────────────────────────────
    # When True, bypasses live price fetch and injects DEMO_OVERRIDE_PRICE
    demo_mode: bool
    demo_override_price: int  # 6dp-scaled


def load_config() -> WatcherConfig:
    """
    Reads all configuration from environment variables.
    Fails fast with a clear message if anything is missing.
    """
    return WatcherConfig(
        rpc_url=_require_env("RPC_URL"),
        contract_address=_require_env("CONTRACT_ADDRESS"),
        signer_mnemonic=_require_env("SIGNER_MNEMONIC"),
        coingecko_coin_id=os.environ.get("COINGECKO_COIN_ID", "polkadot"),
        coingecko_vs_currency=os.environ.get("COINGECKO_VS_CURRENCY", "usd"),
        threshold_price=int(_require_env("THRESHOLD_PRICE")),
        is_below=os.environ.get("IS_BELOW", "true").strip().lower() == "true",
        poll_interval_seconds=int(os.environ.get("POLL_INTERVAL_SECONDS", "30")),
        max_consecutive_failures=int(
            os.environ.get("MAX_CONSECUTIVE_FAILURES", "5")
        ),
        gas_limit=int(os.environ.get("GAS_LIMIT", "5000000000")),
        storage_deposit_limit=None,
        demo_mode=os.environ.get("DEMO_MODE", "false").strip().lower() == "true",
        demo_override_price=int(
            os.environ.get("DEMO_OVERRIDE_PRICE", "0")
        ),
    )