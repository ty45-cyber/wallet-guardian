"""
Wallet Guardian — Off-Chain Watcher
Polls a price feed, evaluates the rule condition, and calls execute_rule
on the ink! contract when the condition is met.

Design principles:
- Watcher is UNTRUSTED by the contract. It passes a price; the contract
  re-validates. A buggy watcher cannot drain funds.
- check_condition() dry-run before every execute_rule() call to avoid
  wasting gas on calls that will revert.
- Exponential backoff on transient failures.
- Single exit point: watcher runs until rule executes OR fatal error.
"""

import logging
import sys
import time
from typing import Optional

import requests
from substrateinterface import SubstrateInterface, Keypair
from substrateinterface.contracts import ContractInstance

from config import WatcherConfig, load_config

# ──────────────────────────────────────────────────────────────────────────────
# Logging — structured, timestamp-prefixed, stdout for container compatibility
# ──────────────────────────────────────────────────────────────────────────────

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    datefmt="%Y-%m-%dT%H:%M:%S",
    handlers=[logging.StreamHandler(sys.stdout)],
)
log = logging.getLogger("wallet_guardian.watcher")


# ──────────────────────────────────────────────────────────────────────────────
# Price Feed
# ──────────────────────────────────────────────────────────────────────────────

COINGECKO_URL = (
    "https://api.coingecko.com/api/v3/simple/price"
    "?ids={coin_id}&vs_currencies={vs_currency}"
)

PRICE_SCALE: Final[int] = 1_000_000  # 6 decimal places


def fetch_price(coin_id: str, vs_currency: str) -> int:
    """
    Fetches current price from CoinGecko and returns it as a 6dp-scaled int.

    Example: $4.27 DOT → 4_270_000

    Raises:
        RuntimeError: on HTTP error or unexpected response shape.
    """
    url = COINGECKO_URL.format(coin_id=coin_id, vs_currency=vs_currency)

    try:
        response = requests.get(url, timeout=10)
        response.raise_for_status()
    except requests.RequestException as exc:
        raise RuntimeError(f"Price fetch HTTP error: {exc}") from exc

    data = response.json()

    try:
        raw_price: float = data[coin_id][vs_currency]
    except (KeyError, TypeError) as exc:
        raise RuntimeError(
            f"Unexpected CoinGecko response shape: {data}"
        ) from exc

    scaled = int(raw_price * PRICE_SCALE)
    log.info("Price fetched: %.6f %s → scaled: %d", raw_price, vs_currency.upper(), scaled)
    return scaled


# ──────────────────────────────────────────────────────────────────────────────
# Condition Evaluation (local mirror of on-chain logic)
# ──────────────────────────────────────────────────────────────────────────────

def condition_met_locally(
    current_price: int,
    threshold_price: int,
    is_below: bool,
) -> bool:
    """
    Local pre-check mirroring the contract's condition logic.
    Saves gas by avoiding calls that will revert with ConditionNotMet.
    """
    if is_below:
        return current_price < threshold_price
    return current_price > threshold_price


# ──────────────────────────────────────────────────────────────────────────────
# Substrate / Contract Interface
# ──────────────────────────────────────────────────────────────────────────────

def build_substrate_client(rpc_url: str) -> SubstrateInterface:
    """Opens and returns a Substrate RPC connection."""
    log.info("Connecting to Substrate node: %s", rpc_url)
    substrate = SubstrateInterface(url=rpc_url)
    log.info(
        "Connected — chain: %s  version: %s",
        substrate.chain,
        substrate.version,
    )
    return substrate


def build_contract(
    substrate: SubstrateInterface,
    contract_address: str,
) -> ContractInstance:
    """
    Loads the deployed contract instance.
    Metadata is fetched on-chain; no local ABI file required.
    """
    log.info("Loading contract at: %s", contract_address)
    contract = ContractInstance.create_from_address(
        contract_address=contract_address,
        metadata_file=None,   # fetched from chain storage
        substrate=substrate,
    )
    return contract


def dry_run_check_condition(
    contract: ContractInstance,
    keypair: Keypair,
    current_price: int,
) -> bool:
    """
    Calls check_condition() as a read-only (dry-run) query.
    Zero gas cost — pure view call.

    Returns True if the contract confirms the condition is met.
    """
    result = contract.read(
        keypair=keypair,
        method="check_condition",
        args={"current_price": current_price},
    )

    # substrateinterface returns result.contract_result_data for ink! reads
    on_chain_result: bool = result.contract_result_data
    log.info(
        "check_condition(price=%d) → on-chain: %s",
        current_price,
        on_chain_result,
    )
    return bool(on_chain_result)


def call_execute_rule(
    substrate: SubstrateInterface,
    contract: ContractInstance,
    keypair: Keypair,
    current_price: int,
    gas_limit: int,
    storage_deposit_limit: Optional[int],
) -> str:
    """
    Submits execute_rule() as a signed extrinsic.

    Returns the extrinsic hash on success.
    Raises RuntimeError if the extrinsic fails or the contract reverts.
    """
    log.info("Submitting execute_rule(current_price=%d) ...", current_price)

    receipt = contract.exec(
        keypair=keypair,
        method="execute_rule",
        args={"current_price": current_price},
        value=0,
        gas_limit={
            "ref_time": gas_limit,
            "proof_size": 131_072,
        },
        storage_deposit_limit=storage_deposit_limit,
        wait_for_finalization=False,  # wait for inclusion only — faster demo
    )

    if receipt.is_success:
        log.info(
            "execute_rule SUCCESS — extrinsic: %s  block: %s",
            receipt.extrinsic_hash,
            receipt.block_hash,
        )

        # Log emitted contract events for clarity
        for event in receipt.contract_events or []:
            log.info("Contract event: %s → %s", event.name, event.value)

        return receipt.extrinsic_hash

    # Contract revert — extract error from receipt
    error_msg = getattr(receipt, "error_message", str(receipt))
    raise RuntimeError(f"execute_rule reverted: {error_msg}")


# ──────────────────────────────────────────────────────────────────────────────
# Main Watch Loop
# ──────────────────────────────────────────────────────────────────────────────

def run_watcher(config: WatcherConfig) -> None:
    """
    Main entry point. Polls price, evaluates condition, executes rule.

    Exits cleanly after successful execution (rule fires once → done).
    Exits with sys.exit(1) on fatal errors.
    """
    log.info("=== Wallet Guardian Watcher starting ===")
    log.info(
        "Config → threshold: %d  is_below: %s  poll: %ds  demo: %s",
        config.threshold_price,
        config.is_below,
        config.poll_interval_seconds,
        config.demo_mode,
    )

    # ── Build clients ──────────────────────────────────────────────────────
    try:
        substrate = build_substrate_client(config.rpc_url)
        keypair = Keypair.create_from_mnemonic(config.signer_mnemonic)
        contract = build_contract(substrate, config.contract_address)
    except Exception as exc:
        log.critical("Startup failed: %s", exc, exc_info=True)
        sys.exit(1)

    log.info("Signer address: %s", keypair.ss58_address)

    consecutive_failures = 0

    # ── Poll loop ──────────────────────────────────────────────────────────
    while True:
        try:
            # ── 1. Fetch or override price ─────────────────────────────────
            if config.demo_mode:
                current_price = config.demo_override_price
                log.info(
                    "[DEMO MODE] Injecting override price: %d", current_price
                )
            else:
                current_price = fetch_price(
                    config.coingecko_coin_id,
                    config.coingecko_vs_currency,
                )

            # ── 2. Local pre-check (gas-free) ──────────────────────────────
            if not condition_met_locally(
                current_price, config.threshold_price, config.is_below
            ):
                direction = "below" if config.is_below else "above"
                log.info(
                    "Condition not met — price %d, waiting for price to go %s %d",
                    current_price,
                    direction,
                    config.threshold_price,
                )
                consecutive_failures = 0
                time.sleep(config.poll_interval_seconds)
                continue

            log.info(
                "Local condition MET — price %d vs threshold %d (is_below=%s)",
                current_price,
                config.threshold_price,
                config.is_below,
            )

            # ── 3. On-chain dry-run (confirm rule still active) ────────────
            on_chain_confirmed = dry_run_check_condition(
                contract, keypair, current_price
            )

            if not on_chain_confirmed:
                log.warning(
                    "On-chain check_condition returned False — "
                    "rule may be inactive or already executed. Stopping."
                )
                sys.exit(0)

            # ── 4. Submit execute_rule extrinsic ───────────────────────────
            extrinsic_hash = call_execute_rule(
                substrate=substrate,
                contract=contract,
                keypair=keypair,
                current_price=current_price,
                gas_limit=config.gas_limit,
                storage_deposit_limit=config.storage_deposit_limit,
            )

            log.info(
                "=== Rule executed successfully. Extrinsic: %s ===",
                extrinsic_hash,
            )
            # Rule fires exactly once. Watcher's job is done.
            sys.exit(0)

        except RuntimeError as exc:
            # Expected operational errors — count toward failure threshold
            consecutive_failures += 1
            log.error(
                "Operational error (%d/%d): %s",
                consecutive_failures,
                config.max_consecutive_failures,
                exc,
            )

            if consecutive_failures >= config.max_consecutive_failures:
                log.critical(
                    "Max consecutive failures reached (%d). Exiting.",
                    config.max_consecutive_failures,
                )
                sys.exit(1)

            backoff = min(
                config.poll_interval_seconds * (2 ** consecutive_failures),
                300,  # cap at 5 minutes
            )
            log.info("Backing off for %ds before retry ...", backoff)
            time.sleep(backoff)

        except KeyboardInterrupt:
            log.info("Watcher stopped by user (KeyboardInterrupt).")
            sys.exit(0)

        except Exception as exc:
            # Unexpected — always fatal
            log.critical("Unexpected fatal error: %s", exc, exc_info=True)
            sys.exit(1)


# ──────────────────────────────────────────────────────────────────────────────
# Entry
# ──────────────────────────────────────────────────────────────────────────────

if __name__ == "__main__":
    cfg = load_config()
    run_watcher(cfg)