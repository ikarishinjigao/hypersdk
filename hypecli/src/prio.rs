//! Gossip priority auction commands.
//!
//! ## What is this?
//!
//! Hyperliquid's gossip network runs **5 Dutch auction slots** (indices 0–4) for
//! read-priority ordering. When you win a slot, your node receives transaction data
//! ~10ms faster per slot level before non-winners see it. All 5 slots reset on the
//! same synchronized 3-minute schedule.
//!
//! The winning bid amount is **burned from your spot HYPE balance**. Any address may
//! bid on behalf of any IP address (the signer doesn't need to own the IP).
//!
//! ## How the Dutch auction works
//!
//! Each slot resets at the start of a cycle. The opening price is **10x the previous
//! cycle's winning price**, decreasing linearly over 180 seconds. Minimum price is
//! **0.1 HYPE**. You pay the price at the moment your bid lands — if it's above the
//! current Dutch auction price, you win and the difference is refunded.
//!
//! Example: If the previous cycle's winning price for slot 0 was 0.05 HYPE, the next
//! cycle opens at 0.5 HYPE and decreases to 0.1 HYPE over 180 seconds.
//!
//! ## Verifying your win
//!
//! After placing a bid, run `hypecli prio status` again and compare:
//! - **currentGas** increased → your bid landed (you won if it equals max_gas)
//! - **currentGas** unchanged → outbid or not yet high enough
//!
//! <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/priority-fees>

use std::io::Write as IoWrite;

use clap::{Args, Subcommand};
use hypersdk::{
    hypercore::{HttpClient, NonceHandler},
    U256,
};
use rust_decimal::Decimal;
use hypersdk::hypercore::types::{OkResponse, Response};

use crate::SignerArgs;
use crate::utils::find_signer_sync;

/// Gossip priority auction commands.
///
/// Run `hypecli prio status` first to see the current Dutch auction prices and
/// time remaining for all 5 slots. Use those prices to decide your `--max` bid.
#[derive(Subcommand)]
pub enum PrioCmd {
    /// Query the current gossip priority auction status.
    ///
    /// Shows Dutch auction parameters and time remaining for all 5 slots.
    /// Use this to decide how much to bid before running `hypecli prio bid`.
    Status(StatusCmd),
    /// Place a signed bid on a gossip priority slot.
    ///
    /// The fee is deducted from your spot HYPE balance and burned. To verify you won,
    /// re-run `hypecli prio status` afterward and look for an updated `currentGas`.
    Bid(BidCmd),
}

impl PrioCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        match self {
            Self::Status(cmd) => cmd.run().await,
            Self::Bid(cmd) => cmd.run().await,
        }
    }
}

/// Query the current gossip priority auction status.
///
/// ## Output fields
///
/// | Column          | Description                                                          |
/// |-----------------|----------------------------------------------------------------------|
/// | Slot            | Index 0–4 (lower = higher priority, ~10ms faster per slot)          |
/// | Start (HYPE)    | Opening price for this auction cycle.                                |
/// | Current (HYPE)  | Live Dutch auction price right now (or "(no bid)" if none landed).   |
/// | End / Min       | Floor price for this slot (typically 0.1 HYPE).                      |
/// | Time Left       | Seconds until the next 3-minute cycle resets.                         |
///
/// ## How to read the prices
///
/// Prices decrease linearly from **Start** to **End** over the 3-minute window.
/// A live price of `(no bid)` with `currentGas: null` means no winner yet this cycle.
/// Winning bids are determined by whichever valid bid arrives first while the price
/// exceeds their `maxGas` limit.
//
// Run this before bidding to gauge competitive prices.
#[derive(Args)]
pub struct StatusCmd {}

impl StatusCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        let client = HttpClient::new(hypersdk::hypercore::Chain::Mainnet);

        println!("Fetching gossip priority auction status...");
        let status = client.gossip_priority_auction_status().await?;

        // Compute overall cycle progress from the first slot.
        let now = chrono::Utc::now().timestamp() as u64;
        let (progress_pct, earliest_end) = if let Some(first) = status.first() {
            let elapsed = now.saturating_sub(first.start_time_seconds);
            let pct = (elapsed as f64 / first.duration_seconds as f64 * 100.0).round() as u32;
            let end = first.start_time_seconds + first.duration_seconds;
            (pct, end)
        } else {
            (0, 0u64)
        };

        let secs_left = earliest_end.saturating_sub(now);

        println!(
            "\nDutch auction status — {}% through {}-second cycle ({}s remaining)",
            progress_pct,
            status.first().map(|s| s.duration_seconds).unwrap_or(180),
            secs_left
        );
        println!(
            "{:<6} {:>12} {:>14} {:>12} {:>12}",
            "Slot", "Start", "Current", "End/Min", "Time"
        );
        println!("{}", "-".repeat(60));

        for (i, slot) in status.iter().enumerate() {
            let elapsed = now.saturating_sub(slot.start_time_seconds);
            let progress = (elapsed as f64 / slot.duration_seconds as f64).clamp(0.0, 1.0);

            // Compute current price from linear interpolation.
            let start: Decimal = slot.start_gas.parse().unwrap_or_default();
            let end: Decimal = slot.end_gas.as_ref().and_then(|s| s.parse().ok()).unwrap_or(start);
            let current_price = start - (start - end) * Decimal::from_f64_retain(progress).unwrap_or_default();

            let current_str = if slot.current_gas.is_some() {
                format!("{:.4}", current_price)
            } else {
                "(no bid)".to_string()
            };
            let time_str = format!("{}s", secs_left);
            let marker = if i == 0 { " ← top" } else { "" };

            println!(
                "{:<6} {:>12} {:>14} {:>12} {:>12}{}",
                i,
                slot.start_gas,
                current_str,
                slot.end_gas.as_deref().unwrap_or("-"),
                time_str,
                marker
            );
        }

        println!(
            "\nTip: Run `hypecli prio status` after bidding and compare `currentGas` \
             to verify your bid landed."
        );

        Ok(())
    }
}

/// Place a signed bid on a gossip priority slot.
///
/// ## How to use
///
/// 1. Run `hypecli prio status` to see current prices.
/// 2. Choose a slot (default: 0, highest priority).
/// 3. Set `--max` to your maximum acceptable price in HYPE units.
/// 4. Set `--ip` to the IP address that will receive prioritized gossip.
///
/// ## How billing works
///
/// - You pay the **current Dutch auction price at submission time**, not your `--max`.
/// - If the current price ≤ `--max`, you win immediately and pay `(current price) × 1e18`
///   wei from your spot HYPE balance.
/// - If the current price > `--max`, your bid is placed but you don't win yet.
///   It stays active for the remainder of the cycle; if the price drops below your
///   max before the cycle ends, you win automatically.
/// - Winning bid amounts are **burned**, not transferred.
///
/// ## Units
///
/// `--max` is in **HYPE** (not wei). 1 HYPE = 10^18 wei.
///
/// ## Examples
///
/// ```bash
/// # Check prices first
/// hypecli prio status
///
/// # Bid 0.5 HYPE max on slot 0 (highest priority) for your public IP.
/// hypecli prio bid --private-key 0x... --max 0.5 --ip 203.0.113.42
///
/// # Lower-priority slot 2, reserve up to 1 HYPE.
/// hypecli prio bid --keystore hot_wallet --max 1.0 --ip 198.51.100.7 --slot 2
/// ```
#[derive(Args, derive_more::Deref)]
pub struct BidCmd {
    #[deref]
    #[command(flatten)]
    pub signer: SignerArgs,

    /// Maximum HYPE to bid, in HYPE units (not wei).
    ///
    /// You pay the current Dutch auction price at submission time — not this value —
    /// as long as it's ≥ the current price. Fees are deducted from your spot HYPE
    /// balance and burned.
    #[arg(long)]
    pub max: Decimal,

    /// IP address to receive prioritized gossip data.
    ///
    /// Any IP may be specified regardless of who signs the transaction. Enter your
    /// node's public IPv4/IPv6 address so the gossip peer can connect directly.
    #[arg(long)]
    pub ip: String,

    /// Slot index to bid on.
    ///
    /// Slots 0–4 exist. Lower index = higher priority (~10 ms latency advantage per
    /// slot level over non-winners). Defaults to slot 0 (top priority).
    ///
    /// | Slot | Priority offset vs no-bid |
    /// |------|---------------------------|
    /// | 0    | ~50 ms faster              |
    /// | 1    | ~40 ms faster              |
    /// | 2    | ~30 ms faster              |
    /// | 3    | ~20 ms faster              |
    /// | 4    | ~10 ms faster              |
    #[arg(long, default_value = "0")]
    pub slot: u8,
}

impl BidCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        let signer = find_signer_sync(&self.signer)?;
        let client = HttpClient::new(self.chain);

        // Convert max HYPE to wei (1 HYPE = 1e18 wei).
        let max_hype: f64 = self
            .max
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid --max value: {}", self.max))?;
        let max_gas = U256::from((max_hype * 1e18) as u128);

        let nonce = NonceHandler::default().next();

        println!("Placing gossip priority bid:");
        println!("  Signer:     {}", signer.address());
        println!("  Slot:       {} ({})", self.slot, if self.slot == 0 { "top priority" } else { "" });
        println!("  Target IP:  {}", self.ip);
        println!("  Max bid:    {} HYPE ({} wei)", self.max, max_gas);
        println!("  Nonce:      {}", nonce);
        println!();

        let resp = client
            .gossip_priority_bid(
                &signer,
                self.slot,
                &self.ip,
                max_gas,
                nonce,
                None,
                None,
            )
            .await?;

        match &resp {
            Response::Ok(OkResponse::Default) => {
                println!("✓ Bid submitted successfully.");
                println!();
                println!("Verify your win by checking `hypecli prio status` —");
                println!("compare the `currentGas` field to confirm your bid landed.");
            }
            Response::Err(err) => {
                println!("✗ Bid failed: {err}");
            }
            _ => {
                println!("Unexpected response: {resp:?}");
            }
        }

        // Refresh and print updated status with price computation.
        println!("\nRefreshing auction status...");
        let new_status = client.gossip_priority_auction_status().await?;

        let now = chrono::Utc::now().timestamp() as u64;

        println!(
            "\n{:<6} {:>12} {:>14} {:>12} {:>12}",
            "Slot", "Start", "Current", "End/Min", "Time"
        );
        println!("{}", "-".repeat(60));

        for (i, slot) in new_status.iter().enumerate() {
            let elapsed = now.saturating_sub(slot.start_time_seconds);
            let progress = (elapsed as f64 / slot.duration_seconds as f64).clamp(0.0, 1.0);

            let start: Decimal = slot.start_gas.parse().unwrap_or_default();
            let end: Decimal = slot.end_gas.as_ref().and_then(|s| s.parse().ok()).unwrap_or(start);
            let current_price = start - (start - end) * Decimal::from_f64_retain(progress).unwrap_or_default();

            let secs_left = slot.start_time_seconds
                .saturating_add(slot.duration_seconds)
                .saturating_sub(now);

            let current_str = if slot.current_gas.is_some() {
                format!("{:.4}", current_price)
            } else {
                "(no bid)".to_string()
            };
            let time_str = format!("{}s", secs_left);

            if i == self.slot as usize {
                writeln!(
                    std::io::stdout(),
                    "{:<6} {:>12} {:>14} {:>12} {:>12} ← target",
                    i, slot.start_gas, current_str, slot.end_gas.as_deref().unwrap_or("-"), time_str
                )?;
            } else {
                writeln!(
                    std::io::stdout(),
                    "{:<6} {:>12} {:>14} {:>12} {:>12}",
                    i, slot.start_gas, current_str, slot.end_gas.as_deref().unwrap_or("-"), time_str
                )?;
            }
        }

        Ok(())
    }
}