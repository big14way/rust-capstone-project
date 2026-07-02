use bitcoincore_rpc::bitcoin::{Address, Amount, Txid};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde_json::{json, Value};
use std::fs::File;
use std::io::Write;

// --- Node connection details (must match docker-compose.yaml / bitcoin.conf) ---
const RPC_URL: &str = "http://127.0.0.1:18443"; // regtest RPC port
const RPC_USER: &str = "alice";
const RPC_PASS: &str = "password";

// A coinbase output cannot be spent until it has this many confirmations, which
// is why the first block reward only becomes spendable after 101 blocks.
const COINBASE_MATURITY: u64 = 100;
// How much the Miner pays the Trader, in whole BTC.
const PAYMENT_BTC: u64 = 20;
// Result file, relative to ./rust (the working dir set by rust/run-rust.sh), so
// it lands in the repo root where the test script reads it.
const OUTPUT_PATH: &str = "../out.txt";

/// The RPC credentials shared by every client we open.
fn rpc_auth() -> Auth {
    Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned())
}

/// Ensure a wallet exists/loaded and return a client bound to it.
///
/// `createwallet`/`loadwallet` are node-level RPCs, so we run them through the
/// node client and then hand back a second client pointed at `/wallet/<name>` —
/// the endpoint that serves wallet RPCs such as `getbalance` and `sendtoaddress`.
fn open_wallet(node: &Client, name: &str) -> bitcoincore_rpc::Result<Client> {
    let already_loaded = node.list_wallets()?.iter().any(|w| w == name);
    // Not loaded yet? Load it from disk, or create it if it doesn't exist.
    if !already_loaded && node.load_wallet(name).is_err() {
        node.create_wallet(name, None, None, None, None)?;
    }
    Client::new(&format!("{RPC_URL}/wallet/{name}"), rpc_auth())
}

/// Mine blocks (one at a time) to `reward` until the Miner wallet has a positive
/// spendable balance, returning how many blocks that took.
///
/// The balance stays at zero while every reward is still an immature coinbase,
/// so on a fresh chain this mines exactly `COINBASE_MATURITY + 1` = 101 blocks:
/// the reward from block 1 only matures once block 101 sits on top of it.
fn mine_until_spendable(
    node: &Client,
    miner: &Client,
    reward: &Address,
) -> bitcoincore_rpc::Result<u64> {
    let mut mined = 0;
    while miner.get_balance(None, None)? == Amount::ZERO {
        node.generate_to_address(1, reward)?;
        mined += 1;
    }
    Ok(mined)
}

/// The ten facts the grader reads back from out.txt, in output order.
struct TransactionReport {
    txid: String,
    miner_input_address: String,
    miner_input_amount: f64,
    trader_output_address: String,
    trader_output_amount: f64,
    miner_change_address: String,
    miner_change_amount: f64,
    fee: f64,
    block_height: i64,
    block_hash: String,
}

impl TransactionReport {
    /// Write each field on its own line, in the order the README specifies.
    fn write_to(&self, path: &str) -> std::io::Result<()> {
        let mut file = File::create(path)?;
        writeln!(file, "{}", self.txid)?;
        writeln!(file, "{}", self.miner_input_address)?;
        writeln!(file, "{}", self.miner_input_amount)?;
        writeln!(file, "{}", self.trader_output_address)?;
        writeln!(file, "{}", self.trader_output_amount)?;
        writeln!(file, "{}", self.miner_change_address)?;
        writeln!(file, "{}", self.miner_change_amount)?;
        writeln!(file, "{}", self.fee)?;
        writeln!(file, "{}", self.block_height)?;
        writeln!(file, "{}", self.block_hash)?;
        Ok(())
    }
}

/// Pull every required detail out of the confirmed payment transaction.
fn collect_report(
    node: &Client,
    miner: &Client,
    txid: &Txid,
    trader_address: &str,
) -> bitcoincore_rpc::Result<TransactionReport> {
    // verbose=true makes gettransaction include the fully `decoded` transaction
    // alongside the wallet fee and the confirming block's hash and height.
    let tx: Value = miner.call(
        "gettransaction",
        &[json!(txid.to_string()), Value::Null, json!(true)],
    )?;
    let decoded = &tx["decoded"];

    // The single input spends a coinbase; resolve that previous output to read
    // its address and amount (txindex=1 lets us fetch any tx by id).
    let input = &decoded["vin"][0];
    let prev_tx: Value = node.call("getrawtransaction", &[input["txid"].clone(), json!(true)])?;
    let spent = &prev_tx["vout"][input["vout"].as_u64().unwrap() as usize];

    // The two outputs are the Trader payment and the Miner's change; tell them
    // apart by matching the Trader's address.
    let mut trader_amount = 0.0;
    let mut change_address = String::new();
    let mut change_amount = 0.0;
    for output in decoded["vout"].as_array().unwrap() {
        let address = output["scriptPubKey"]["address"].as_str().unwrap();
        let value = output["value"].as_f64().unwrap();
        if address == trader_address {
            trader_amount = value;
        } else {
            change_address = address.to_owned();
            change_amount = value;
        }
    }

    Ok(TransactionReport {
        txid: txid.to_string(),
        miner_input_address: spent["scriptPubKey"]["address"]
            .as_str()
            .unwrap()
            .to_owned(),
        miner_input_amount: spent["value"].as_f64().unwrap(),
        trader_output_address: trader_address.to_owned(),
        trader_output_amount: trader_amount,
        miner_change_address: change_address,
        miner_change_amount: change_amount,
        fee: tx["fee"].as_f64().unwrap(),
        block_height: tx["blockheight"].as_i64().unwrap(),
        block_hash: tx["blockhash"].as_str().unwrap().to_owned(),
    })
}

fn main() -> bitcoincore_rpc::Result<()> {
    // Node-level client (not bound to any wallet).
    let node = Client::new(RPC_URL, rpc_auth())?;
    println!(
        "Connected to node on {:?}",
        node.get_blockchain_info()?.chain
    );

    // Step 1: get the two wallets ready (names are case-sensitive).
    let miner = open_wallet(&node, "Miner")?;
    let trader = open_wallet(&node, "Trader")?;

    // Step 2: mine to a Miner address until the first reward matures.
    let reward_address = miner
        .get_new_address(Some("Mining Reward"), None)?
        .assume_checked();
    let blocks = mine_until_spendable(&node, &miner, &reward_address)?;
    let balance = miner.get_balance(None, None)?;
    println!("Mined {blocks} blocks (coinbase maturity is {COINBASE_MATURITY}).");
    println!("Miner spendable balance is now {} BTC.", balance.to_btc());

    // Step 3: pay 20 BTC from Miner to a fresh Trader address. The Miner holds a
    // single mature 50 BTC coinbase, so this spends 1 input and creates 2
    // outputs: the payment plus change back to the Miner.
    let trader_address = trader
        .get_new_address(Some("Received"), None)?
        .assume_checked();
    let payment = Amount::from_sat(PAYMENT_BTC * Amount::ONE_BTC.to_sat());
    let txid =
        miner.send_to_address(&trader_address, payment, None, None, None, None, None, None)?;
    println!("Broadcast {PAYMENT_BTC} BTC payment: {txid}");

    // Step 4: read it back from the mempool while it is still unconfirmed.
    println!("Mempool entry: {:?}", node.get_mempool_entry(&txid)?);

    // Step 5: confirm it by mining a single block.
    node.generate_to_address(1, &reward_address)?;

    // Step 6: collect the details and write them out for the grader.
    let report = collect_report(&node, &miner, &txid, &trader_address.to_string())?;
    report.write_to(OUTPUT_PATH)?;
    println!("Wrote transaction report to {OUTPUT_PATH}");

    Ok(())
}
