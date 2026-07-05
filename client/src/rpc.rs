//! Minimal raw JSON-RPC over reqwest (same rail copybot-rs uses — no heavy
//! solana-client dependency). Local-validator test edition: adds airdrop +
//! rent query.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_sdk::{hash::Hash, pubkey::Pubkey, transaction::Transaction};
use std::str::FromStr;

pub struct Rpc {
    url: String,
    client: reqwest::Client,
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        Self { url: url.to_string(), client: reqwest::Client::new() }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        match self.client.post(&self.url).json(&body).send().await {
            Ok(r) => r.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        }
    }

    pub async fn blockhash(&self) -> Hash {
        let v = self.call("getLatestBlockhash", serde_json::json!([{"commitment":"confirmed"}])).await;
        let s = v["result"]["value"]["blockhash"].as_str().unwrap_or_default();
        Hash::from_str(s).unwrap_or_default()
    }

    pub async fn balance(&self, pk: &Pubkey) -> u64 {
        let v = self.call("getBalance", serde_json::json!([pk.to_string(), {"commitment":"confirmed"}])).await;
        v["result"]["value"].as_u64().unwrap_or(0)
    }

    pub async fn account_data(&self, pk: &Pubkey) -> Option<Vec<u8>> {
        let v = self.call("getAccountInfo", serde_json::json!([pk.to_string(), {"encoding":"base64","commitment":"confirmed"}])).await;
        let d = v["result"]["value"]["data"].get(0)?.as_str()?;
        B64.decode(d).ok()
    }

    pub async fn min_balance(&self, size: usize) -> u64 {
        let v = self.call("getMinimumBalanceForRentExemption", serde_json::json!([size])).await;
        v["result"].as_u64().unwrap_or(0)
    }

    /// Local validator faucet. Waits until the balance lands.
    pub async fn airdrop(&self, pk: &Pubkey, lamports: u64) -> Result<(), String> {
        let before = self.balance(pk).await;
        let v = self.call("requestAirdrop", serde_json::json!([pk.to_string(), lamports])).await;
        if v.get("error").is_some() {
            return Err(v["error"]["message"].as_str().unwrap_or("airdrop error").to_string());
        }
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if self.balance(pk).await >= before + lamports {
                return Ok(());
            }
        }
        Err("airdrop timeout".into())
    }

    /// Send + confirm. Returns Ok(sig) or Err(on-chain error string).
    pub async fn send(&self, tx: &Transaction) -> Result<String, String> {
        let wire = bincode::serialize(tx).map_err(|e: Box<bincode::ErrorKind>| e.to_string())?;
        let b64 = B64.encode(wire);
        let v = self.call("sendTransaction", serde_json::json!([b64, {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","maxRetries":5}])).await;
        if let Some(err) = v.get("error") {
            return Err(err["message"].as_str().unwrap_or("send error").to_string());
        }
        let sig = v["result"].as_str().ok_or("no signature")?.to_string();
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let s = self.call("getSignatureStatuses", serde_json::json!([[sig], {"searchTransactionHistory":false}])).await;
            let st = &s["result"]["value"][0];
            if st.is_null() {
                continue;
            }
            if !st["err"].is_null() {
                return Err(format!("tx failed: {}", st["err"]));
            }
            let cs = st["confirmationStatus"].as_str().unwrap_or("");
            if cs == "confirmed" || cs == "finalized" {
                return Ok(sig);
            }
        }
        Err("confirmation timeout".into())
    }
}
