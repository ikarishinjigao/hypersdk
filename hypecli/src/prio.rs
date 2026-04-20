//! Gossip priority auction commands.
//!
//! This module provides commands for:
//! - Querying the current priority auction status (winning prices, time remaining)
//! - Placing a signed bid to win a priority slot
//!
//! The Hyperliquid gossip network runs a Dutch auction for 5 priority slots (0–4).
//! Slot 0 has the highest priority (~10ms latency advantage per slot). Each slot
//! resets on a 3-minute synchronized schedule. The winning bid amount is burned
//! from your **spot HYPE balance**.

use std::io::Write;

use clap::{Args, Subcommand};
use hypersdk::{
    hypercore::{HttpClient, NonceHandler, types::{OkResponse, Response}},
    U256,
};
use rust_decimal::Decimal;

use crate::SignerArgs;
use crate::utils::find_signer_sync;

/// Gossip priority auction commands.
///
/// Use these to bid on priority slots for faster gossip delivery.
#[derive(Subcommand)]
pub enum PrioCmd {
    /// Query the current auction status (winning prices, time remaining).
    Status(StatusCmd),
    /// Place a signed bid on a priority slot.
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
/// Displays the winning price, time remaining, and current winner for all
/// 5 slots (0–4). Use this to decide how much to bid.
///
/// # Example
///
/// ```bash
/// hypecli prio status
/// ```
#[derive(Args)]
pub struct StatusCmd {}

impl StatusCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        let client = HttpClient::new(hypersdk::hypercore::Chain::Mainnet);

        println!("Fetching gossip priority auction status...");
        let status = client.gossip_priority_auction_status().await?;

        println!("\nCurrent auction status (3-minute cycle):");
        println!(
            "{:<6} {:>16} {:>14} {}",
            "Slot", "Price (HYPE)", "Time Left", "Winner"
        );
        println!("{}", "-".repeat(60));

        for slot in &status.slots {
            let time = if slot.secs_remaining == 0 {
                "Expired".to_string()
            } else {
                format!("{}s", slot.secs_remaining)
            };
            println!(
                "{:<6} {:>16} {:>14} {}",
                slot.slot_id,
                slot.price,
                time,
                if slot.winner.is_empty() {
                    "(none)"
                } else {
                    &slot.winner
                }
            );
        }

        Ok(())
    }
}

/// Place a signed bid on a gossip priority slot.
///
/// The bid uses a Dutch auction: the price decreases over the 3-minute cycle.
/// Your maximum bid (`--max`) is the most you're willing to pay; you pay the
/// final auction price, deducted from your **spot HYPE balance** and burned.
///
/// Slot 0 = highest priority (~10ms faster than slot 1, ~20ms faster than slot 2, etc.)
///
/// # Example
///
/// ```bash
/// # Bid 0.5 HYPE on slot 0 for the caller's public IP.
/// hypecli prio bid --private-key 0x... --max 0.5 --ip 1.2.3.4
///
/// # Bid on slot 1 with a Foundry keystore.
/// hypecli prio bid --keystore hot_wallet --max 1.0 --ip 203.0.113.42 --slot 1
/// ```
///
/// --max is in HYPE units (not wei). 1 HYPE = 1e18 wei.
#[derive(Args, derive_more::Deref)]
pub struct BidCmd {
    #[deref]
    #[command(flatten)]
    pub signer: SignerArgs,

    /// Maximum HYPE to bid (in HYPE, not wei). The actual price is determined
    /// by the Dutch auction; you pay the final price, not this amount.
    #[arg(long)]
    pub max: Decimal,

    /// IP address to receive prioritized gossip data.
    #[arg(long)]
    pub ip: String,

    /// Slot index to bid on (0=highest priority, 4=lowest). Defaults to 0.
    #[arg(long, default_value = "0")]
    pub slot: u8,
}

impl BidCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        let signer = find_signer_sync(&self.signer)?;
        let client = HttpClient::new(self.chain);

        // Convert max HYPE to wei.
        // Decimal can't directly do * 1e18, so we use f64 as an intermediate.
        let max_hype: f64 = self
            .max
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid --max value: {}", self.max))?;
        let max_gas = U256::from((max_hype * 1e18) as u128);

        let nonce = NonceHandler::default().next();

        println!(
            "Bidding on slot {} for IP {} with max {} HYPE ({} wei)",
            self.slot, self.ip, self.max, max_gas
        );
        println!("Signer: {}", signer.address());

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
                println!("Bid submitted successfully.");
            }
            Response::Err(err) => {
                println!("Bid failed: {err}");
            }
            _ => {
                println!("Response: {resp:?}");
            }
        }

        // Print updated status so the caller can verify.
        println!("\nRefreshing auction status...");
        let new_status = client.gossip_priority_auction_status().await?;

        println!(
            "\n{:<6} {:>16} {:>14} {}",
            "Slot", "Price (HYPE)", "Time Left", "Winner"
        );
        println!("{}", "-".repeat(60));

        for slot in &new_status.slots {
            let time = if slot.secs_remaining == 0 {
                "Expired".to_string()
            } else {
                format!("{}s", slot.secs_remaining)
            };
            writeln!(
                std::io::stdout(),
                "{:<6} {:>16} {:>14} {}",
                slot.slot_id,
                slot.price,
                time,
                if slot.winner.is_empty() {
                    "(none)"
                } else {
                    &slot.winner
                }
            )?;
        }

        Ok(())
    }
}