use anyhow::{bail, Context, Result};
use bitcoin::{consensus::{deserialize, encode::serialize_hex}, hashes::hex::FromHex, BlockHash, Txid, Amount};
use crossbeam_channel::Receiver;
use rayon::prelude::*;
use serde_derive::Deserialize;
use serde_json::{self, json, Value};
use std::collections::{hash_map::Entry, HashMap};
use std::iter::FromIterator;

use crate::{
    cache::Cache,
    config::{Config, ELECTRS_VERSION},
    daemon::{self, extract_bitcoind_error, Daemon},
    merkle::Proof,
    metrics::{self, Histogram, Metrics},
    signals::Signal,
    status::ScriptHashStatus,
    tracker::Tracker,
    types::ScriptHash,
};
use crate::status::UnspentEntry;

const PROTOCOL_VERSION: &str = "1.4";
const UNKNOWN_FEE: isize = -1; // (allowed by Electrum protocol)

const UNSUBSCRIBED_QUERY_MESSAGE: &str = "your wallet uses less efficient method of querying electrs, consider contacting the developer of your wallet. Reason:";

/// Per-client Electrum protocol state
#[derive(Default)]
pub struct Client {
    tip: Option<BlockHash>,
    scripthashes: HashMap<ScriptHash, ScriptHashStatus>,
}

#[derive(Deserialize)]
struct Request {
    id: Value,
    method: String,

    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Requests {
    Single(Request),
    Batch(Vec<Request>),
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(untagged)]
enum Version {
    Single(String),
    Range(String, String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TxGetArgs {
    Txid((Txid,)),
    TxidVerbose(Txid, bool),
}

impl From<&TxGetArgs> for (Txid, bool) {
    fn from(args: &TxGetArgs) -> Self {
        match args {
            TxGetArgs::Txid((txid,)) => (*txid, false),
            TxGetArgs::TxidVerbose(txid, verbose) => (*txid, *verbose),
        }
    }
}

enum StandardError {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
}

enum RpcError {
    // JSON-RPC spec errors
    Standard(StandardError),
    // Electrum-specific errors
    BadRequest(anyhow::Error),
    DaemonError(daemon::RpcError),
    UnavailableIndex,
}

impl RpcError {
    fn to_value(&self) -> Value {
        match self {
            RpcError::Standard(err) => match err {
                StandardError::ParseError => json!({"code": -32700, "message": "parse error"}),
                StandardError::InvalidRequest => {
                    json!({"code": -32600, "message": "invalid request"})
                }
                StandardError::MethodNotFound => {
                    json!({"code": -32601, "message": "method not found"})
                }
                StandardError::InvalidParams => {
                    json!({"code": -32602, "message": "invalid params"})
                }
            },
            RpcError::BadRequest(err) => json!({"code": 1, "message": err.to_string()}),
            RpcError::DaemonError(err) => json!({"code": 2, "message": err.message}),
            RpcError::UnavailableIndex => {
                // Internal JSON-RPC error (https://www.jsonrpc.org/specification#error_object)
                json!({"code": -32603, "message": "unavailable index"})
            }
        }
    }
}

/// Electrum RPC handler
pub struct Rpc {
    tracker: Tracker,
    cache: Cache,
    rpc_duration: Histogram,
    daemon: Daemon,
    signal: Signal,
    banner: String,
    port: u16,
}

impl Rpc {
    /// Perform initial index sync (may take a while on first run).
    pub fn new(config: &Config, metrics: Metrics) -> Result<Self> {
        let rpc_duration = metrics.histogram_vec(
            "rpc_duration",
            "RPC duration (in seconds)",
            "method",
            metrics::default_duration_buckets(),
        );

        let tracker = Tracker::new(config, metrics)?;
        let signal = Signal::new();
        let daemon = Daemon::connect(config, signal.exit_flag(), tracker.metrics())?;
        let cache = Cache::new(tracker.metrics());
        Ok(Self {
            tracker,
            cache,
            rpc_duration,
            daemon,
            signal,
            banner: config.server_banner.clone(),
            port: config.electrum_rpc_addr.port(),
        })
    }

    pub(crate) fn signal(&self) -> &Signal {
        &self.signal
    }

    pub fn new_block_notification(&self) -> Receiver<()> {
        self.daemon.new_block_notification()
    }

    pub fn sync(&mut self) -> Result<bool> {
        self.tracker.sync(&self.daemon, self.signal.exit_flag())
    }

    pub fn update_client(&self, client: &mut Client) -> Result<Vec<String>> {
        let chain = self.tracker.chain();
        let mut notifications = client
            .scripthashes
            .par_iter_mut()
            .filter_map(|(scripthash, status)| -> Option<Result<Value>> {
                match self
                    .tracker
                    .update_scripthash_status(status, &self.daemon, &self.cache)
                {
                    Ok(true) => Some(Ok(notification(
                        "blockchain.scripthash.subscribe",
                        &[json!(scripthash), json!(status.statushash())],
                    ))),
                    Ok(false) => None, // statushash is the same
                    Err(e) => Some(Err(e)),
                }
            })
            .collect::<Result<Vec<Value>>>()
            .context("failed to update status")?;

        if let Some(old_tip) = client.tip {
            let new_tip = self.tracker.chain().tip();
            if old_tip != new_tip {
                client.tip = Some(new_tip);
                let height = chain.height();
                let header = chain.get_block_header(height).unwrap();
                notifications.push(notification(
                    "blockchain.headers.subscribe",
                    &[json!({"hex": serialize_hex(&header), "height": height})],
                ));
            }
        }
        Ok(notifications.into_iter().map(|v| v.to_string()).collect())
    }

    fn headers_subscribe(&self, client: &mut Client) -> Result<Value> {
        let chain = self.tracker.chain();
        client.tip = Some(chain.tip());
        let height = chain.height();
        let header = chain.get_block_header(height).unwrap();
        Ok(json!({"hex": serialize_hex(header), "height": height}))
    }

    fn block_header(&self, (height,): (usize,)) -> Result<Value> {
        let chain = self.tracker.chain();
        let header = match chain.get_block_header(height) {
            None => bail!("no header at {}", height),
            Some(header) => header,
        };
        Ok(json!(serialize_hex(header)))
    }

    fn block_headers(&self, (start_height, count): (usize, usize)) -> Result<Value> {
        let chain = self.tracker.chain();
        let max_count = 2016usize;
        // return only the available block headers
        let end_height = std::cmp::min(
            chain.height() + 1,
            start_height + std::cmp::min(count, max_count),
        );
        let heights = start_height..end_height;
        let count = heights.len();
        let hex_headers =
            heights.filter_map(|height| chain.get_block_header(height).map(serialize_hex));

        Ok(json!({"count": count, "hex": String::from_iter(hex_headers), "max": max_count}))
    }

    fn estimate_fee(&self, (nblocks,): (u16,)) -> Result<Value> {
        Ok(self
            .daemon
            .estimate_fee(nblocks)?
            .map(|fee_rate| json!(fee_rate.to_btc()))
            .unwrap_or_else(|| json!(UNKNOWN_FEE)))
    }

    fn relayfee(&self) -> Result<Value> {
        Ok(json!(self.daemon.get_relay_fee()?.to_btc())) // [BTC/kB]
    }

    fn scripthash_get_balance(
        &self,
        client: &Client,
        (scripthash,): &(ScriptHash,),
    ) -> Result<Value> {
        let balance = match client.scripthashes.get(scripthash) {
            Some(status) => self.tracker.get_balance(status),
            None => {
                info!(
                    "{} blockchain.scripthash.get_balance called for unsubscribed scripthash",
                    UNSUBSCRIBED_QUERY_MESSAGE
                );
                self.tracker.get_balance(&self.new_status(*scripthash)?)
            }
        };
        Ok(json!(balance))
    }

    fn scripthash_get_history(
        &self,
        client: &Client,
        (scripthash, ): &(ScriptHash, ),
    ) -> Result<Value> {
        let history_entries = match client.scripthashes.get(scripthash) {
            Some(status) => json!(status.get_history(&None, &None)),
            None => {
                info!(
                    "{} blockchain.scripthash.get_history called for unsubscribed scripthash",
                    UNSUBSCRIBED_QUERY_MESSAGE
                );
                json!(self.new_status(*scripthash)?.get_history(&None, &None))
            }
        };
        Ok(history_entries)
    }

    fn scripthash_get_history_filter(
        &self,
        client: &Client,
        (scripthash, from, to): &(ScriptHash, Option<usize>, Option<usize>),
    ) -> Result<Value> {
        let history_entries = match client.scripthashes.get(scripthash) {
            Some(status) => json!(status.get_history(from, to)),
            None => {
                info!(
                    "{} blockchain.scripthash.get_history called for unsubscribed scripthash: {}",
                    UNSUBSCRIBED_QUERY_MESSAGE, scripthash
                );
                json!(self.new_status(*scripthash)?.get_history(from, to))
            }
        };
        Ok(history_entries)
    }

    fn scripthash_list_unspent(
        &self,
        client: &Client,
        (scripthash,): &(ScriptHash,),
    ) -> Result<Value> {
        let unspent_entries = match client.scripthashes.get(scripthash) {
            Some(status) => self.tracker.get_unspent(status),
            None => {
                info!(
                    "{} blockchain.scripthash.listunspent called for unsubscribed scripthash",
                    UNSUBSCRIBED_QUERY_MESSAGE
                );
                self.tracker.get_unspent(&self.new_status(*scripthash)?)
            }
        };
        Ok(json!(unspent_entries))
    }

    fn scripthash_select_unspent(
        &self,
        client: &Client,
        (scripthash, amounts, min_amount, confirmed): &(ScriptHash, Vec<u64>, u64, bool),
    ) -> Result<Value> {
        let mut unspent_entries = match client.scripthashes.get(scripthash) {
            Some(status) => self.tracker.get_unspent(status),
            None => {
                info!(
                "{} blockchain.scripthash.listunspent called for unsubscribed scripthash",
                UNSUBSCRIBED_QUERY_MESSAGE
            );
                self.tracker.get_unspent(&self.new_status(*scripthash)?)
            }
        };
        let filter_confirmed = |utxo: &UnspentEntry, confirmed| {
            if confirmed {
                utxo.height > 0
            } else {
                true
            }
        };
        unspent_entries.retain(|utxo| utxo.value >= Amount::from_sat(*min_amount) && filter_confirmed(utxo, *confirmed));
        unspent_entries.sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap());

        let mut choose_list = Vec::new();
        for target_amount in amounts {
            let (mut part_choose_list, part_index) = select_utxos(&unspent_entries, Amount::from_sat(*target_amount));
            for (iter_index, selcet_index) in part_index.iter().enumerate() {
                unspent_entries.remove(selcet_index - iter_index);
            }
            choose_list.append(&mut part_choose_list);
        }
        info!(
            "choose_list len for req: {:?}", choose_list.len(),
        );
        Ok(json!(choose_list))
    }

    fn scripthash_unspent_is_exist(
        &self,
        client: &Client,
        (scripthash, tx_id): &(ScriptHash, Txid),
    ) -> Result<Value> {
        let unspent_entries = match client.scripthashes.get(scripthash) {
            Some(status) => self.tracker.get_unspent(status),
            None => {
                info!(
                    "{} blockchain.scripthash.listunspent called for unsubscribed scripthash: {}",
                    UNSUBSCRIBED_QUERY_MESSAGE, scripthash
                );
                self.tracker.get_unspent(&self.new_status(*scripthash)?)
            }
        };
        let is_exist = unspent_entries.iter().find(|unspent| &unspent.tx_hash == tx_id).is_some();
        Ok(json!(is_exist))
    }

    fn scripthash_subscribe(
        &self,
        client: &mut Client,
        (scripthash,): &(ScriptHash,),
    ) -> Result<Value> {
        self.scripthashes_subscribe(client, &[*scripthash])
            .next()
            .unwrap()
    }

    fn scripthash_unsubscribe(
        &self,
        client: &mut Client,
        (scripthash,): &(ScriptHash,),
    ) -> Result<Value> {
        let removed = client.scripthashes.remove(scripthash).is_some();
        Ok(json!(removed))
    }

    fn scripthashes_subscribe<'a>(
        &self,
        client: &'a mut Client,
        scripthashes: &'a [ScriptHash],
    ) -> impl Iterator<Item = Result<Value>> + 'a {
        let new_scripthashes: Vec<ScriptHash> = scripthashes
            .iter()
            .copied()
            .filter(|scripthash| !client.scripthashes.contains_key(scripthash))
            .collect();

        let mut results: HashMap<ScriptHash, Result<ScriptHashStatus>> = new_scripthashes
            .into_par_iter()
            .map(|scripthash| (scripthash, self.new_status(scripthash)))
            .collect();

        scripthashes.iter().map(move |scripthash| {
            let statushash = match client.scripthashes.entry(*scripthash) {
                Entry::Occupied(e) => e.get().statushash(),
                Entry::Vacant(e) => {
                    let status = results
                        .remove(scripthash)
                        .expect("missing scripthash status")?; // return an error for failed subscriptions
                    e.insert(status).statushash()
                }
            };
            Ok(json!(statushash))
        })
    }

    fn new_status(&self, scripthash: ScriptHash) -> Result<ScriptHashStatus> {
        let mut status = ScriptHashStatus::new(scripthash);
        self.tracker
            .update_scripthash_status(&mut status, &self.daemon, &self.cache)?;
        Ok(status)
    }

    fn transaction_broadcast(&self, (tx_hex,): &(String,)) -> Result<Value> {
        let tx_bytes = Vec::from_hex(tx_hex).context("non-hex transaction")?;
        let tx = deserialize(&tx_bytes).context("invalid transaction")?;
        let txid = self.daemon.broadcast(&tx)?;
        Ok(json!(txid))
    }

    fn transaction_get(&self, args: &TxGetArgs) -> Result<Value> {
        let (txid, verbose) = args.into();
        if verbose {
            let blockhash = self
                .tracker
                .lookup_transaction(&self.daemon, txid)?
                .map(|(blockhash, _tx)| blockhash);
            return self.daemon.get_transaction_info(&txid, blockhash);
        }
        if let Some(tx) = self.cache.get_tx(&txid, |tx| serialize_hex(tx)) {
            return Ok(json!(tx));
        }
        debug!("tx cache miss: txid={}", txid);
        // use internal index to load confirmed transaction without an RPC
        if let Some(tx) = self
            .tracker
            .lookup_transaction(&self.daemon, txid)?
            .map(|(_blockhash, tx)| tx)
        {
            return Ok(json!(serialize_hex(&tx)));
        }
        // load unconfirmed transaction via RPC
        Ok(json!(self.daemon.get_transaction_hex(&txid, None)?))
    }

    fn transaction_get_merkle(&self, (txid, height): &(Txid, usize)) -> Result<Value> {
        let chain = self.tracker.chain();
        let blockhash = match chain.get_block_hash(*height) {
            None => bail!("missing block at {}", height),
            Some(blockhash) => blockhash,
        };
        let txids = self.daemon.get_block_txids(blockhash)?;
        match txids.iter().position(|current_txid| *current_txid == *txid) {
            None => bail!("missing txid {} in block {}", txid, blockhash),
            Some(position) => {
                let proof = Proof::create(&txids, position);
                Ok(json!({
                "block_height": height,
                "pos": proof.position(),
                "merkle": proof.to_hex(),
                }))
            }
        }
    }

    fn transaction_from_pos(
        &self,
        (height, tx_pos, merkle): (usize, usize, bool),
    ) -> Result<Value> {
        let chain = self.tracker.chain();
        let blockhash = match chain.get_block_hash(height) {
            None => bail!("missing block at {}", height),
            Some(blockhash) => blockhash,
        };
        let txids = self.daemon.get_block_txids(blockhash)?;
        if tx_pos >= txids.len() {
            bail!("invalid tx_pos {} in block at height {}", tx_pos, height);
        }
        let txid: Txid = txids[tx_pos];
        if merkle {
            let proof = Proof::create(&txids, tx_pos);
            Ok(json!({"tx_id": txid, "merkle": proof.to_hex()}))
        } else {
            Ok(json!({ "tx_id": txid }))
        }
    }

    fn get_fee_histogram(&self) -> Result<Value> {
        Ok(json!(self.tracker.fees_histogram()))
    }

    fn server_id(&self) -> String {
        format!("electrs/{}", ELECTRS_VERSION)
    }

    fn version(&self, (client_id, client_version): &(String, Version)) -> Result<Value> {
        match client_version {
            Version::Single(v) if v == PROTOCOL_VERSION => {
                Ok(json!([self.server_id(), PROTOCOL_VERSION]))
            }
            _ => {
                bail!(
                    "{} requested {:?}, server supports {}",
                    client_id,
                    client_version,
                    PROTOCOL_VERSION
                );
            }
        }
    }

    fn features(&self) -> Result<Value> {
        Ok(json!({
            "genesis_hash": self.tracker.chain().get_block_hash(0),
            "hosts": { "tcp_port": self.port },
            "protocol_max": PROTOCOL_VERSION,
            "protocol_min": PROTOCOL_VERSION,
            "pruning": null,
            "server_version": self.server_id(),
            "hash_function": "sha256"
        }))
    }

    pub fn handle_requests(&self, client: &mut Client, lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                parse_requests(line)
                    .map(Calls::parse)
                    .map_err(error_msg_no_id)
            })
            .map(|calls| self.handle_calls(client, calls).to_string())
            .collect()
    }

    fn handle_calls(&self, client: &mut Client, calls: Result<Calls, Value>) -> Value {
        let calls: Calls = match calls {
            Ok(calls) => calls,
            Err(response) => return response, // JSON parsing failed - the response does not contain request id
        };

        match calls {
            Calls::Batch(batch) => {
                if let Some(result) = self.try_multi_call(client, &batch) {
                    return json!(result);
                }
                json!(batch
                    .into_iter()
                    .map(|result| self.single_call(client, result))
                    .collect::<Vec<Value>>())
            }
            Calls::Single(result) => self.single_call(client, result),
        }
    }

    fn try_multi_call(
        &self,
        client: &mut Client,
        calls: &[Result<Call, Value>],
    ) -> Option<Vec<Value>> {
        // exit if any call failed to parse
        let valid_calls = calls
            .iter()
            .map(|result| result.as_ref().ok())
            .collect::<Option<Vec<&Call>>>()?;

        // only "blockchain.scripthashes.subscribe" are supported
        let scripthashes: Vec<ScriptHash> = valid_calls
            .iter()
            .map(|call| match &call.params {
                Params::ScriptHashSubscribe((scripthash,)) => Some(*scripthash),
                _ => None, // exit if any of the calls is not supported
            })
            .collect::<Option<Vec<ScriptHash>>>()?;

        Some(
            self.rpc_duration
                .observe_duration("blockchain.scripthash.subscribe:multi", || {
                    self.scripthashes_subscribe(client, &scripthashes)
                        .zip(valid_calls)
                        .map(|(result, call)| call.response(result))
                        .collect::<Vec<Value>>()
                }),
        )
    }

    fn single_call(&self, client: &mut Client, call: Result<Call, Value>) -> Value {
        let call = match call {
            Ok(call) => call,
            Err(response) => return response, // params parsing may fail - the response contains request id
        };
        self.rpc_duration.observe_duration(&call.method, || {
            if self.tracker.status().is_err() {
                // Allow only a few RPC (for sync status notification) not requiring index DB being compacted.
                match &call.params {
                    Params::BlockHeader(_)
                    | Params::BlockHeaders(_)
                    | Params::HeadersSubscribe
                    | Params::Version(_) => (),
                    _ => return error_msg(&call.id, RpcError::UnavailableIndex),
                };
            }
            let result = match &call.params {
                Params::Banner => Ok(json!(self.banner)),
                Params::BlockHeader(args) => self.block_header(*args),
                Params::BlockHeaders(args) => self.block_headers(*args),
                Params::Donation => Ok(Value::Null),
                Params::EstimateFee(args) => self.estimate_fee(*args),
                Params::Features => self.features(),
                Params::HeadersSubscribe => self.headers_subscribe(client),
                Params::MempoolFeeHistogram => self.get_fee_histogram(),
                Params::PeersSubscribe => Ok(json!([])),
                Params::Ping => Ok(Value::Null),
                Params::RelayFee => self.relayfee(),
                Params::ScriptHashGetBalance(args) => self.scripthash_get_balance(client, args),
                Params::ScriptHashGetHistory(args) => self.scripthash_get_history(client, args),
                Params::ScriptHashGetHistoryFilter(args) => self.scripthash_get_history_filter(client, args),
                Params::ScriptHashListUnspent(args) => self.scripthash_list_unspent(client, args),
                Params::ScriptHashSelectUnspent(args) => self.scripthash_select_unspent(client, args),
                Params::ScriptHashUnspentExist(args) => self.scripthash_unspent_is_exist(client, args),
                Params::ScriptHashSubscribe(args) => self.scripthash_subscribe(client, args),
                Params::ScriptHashUnsubscribe(args) => self.scripthash_unsubscribe(client, args),
                Params::TransactionBroadcast(args) => self.transaction_broadcast(args),
                Params::TransactionGet(args) => self.transaction_get(args),
                Params::TransactionGetMerkle(args) => self.transaction_get_merkle(args),
                Params::TransactionFromPosition(args) => self.transaction_from_pos(*args),
                Params::Version(args) => self.version(args),
            };
            call.response(result)
        })
    }
}

#[derive(Deserialize)]
enum Params {
    Banner,
    BlockHeader((usize,)),
    BlockHeaders((usize, usize)),
    TransactionBroadcast((String,)),
    Donation,
    EstimateFee((u16,)),
    Features,
    HeadersSubscribe,
    MempoolFeeHistogram,
    PeersSubscribe,
    Ping,
    RelayFee,
    ScriptHashGetBalance((ScriptHash,)),
    ScriptHashGetHistory((ScriptHash, )),
    ScriptHashGetHistoryFilter((ScriptHash, Option<usize>, Option<usize>, )),
    ScriptHashListUnspent((ScriptHash,)),
    ScriptHashSelectUnspent((ScriptHash, Vec<u64>, u64, bool, )),
    ScriptHashUnspentExist((ScriptHash, Txid, )),
    ScriptHashSubscribe((ScriptHash,)),
    ScriptHashUnsubscribe((ScriptHash,)),
    TransactionGet(TxGetArgs),
    TransactionGetMerkle((Txid, usize)),
    TransactionFromPosition((usize, usize, bool)),
    Version((String, Version)),
}

impl Params {
    fn parse(method: &str, params: Value) -> std::result::Result<Params, StandardError> {
        Ok(match method {
            "blockchain.block.header" => Params::BlockHeader(convert(params)?),
            "blockchain.block.headers" => Params::BlockHeaders(convert(params)?),
            "blockchain.estimatefee" => Params::EstimateFee(convert(params)?),
            "blockchain.headers.subscribe" => Params::HeadersSubscribe,
            "blockchain.relayfee" => Params::RelayFee,
            "blockchain.scripthash.get_balance" => Params::ScriptHashGetBalance(convert(params)?),
            "blockchain.scripthash.get_history" => Params::ScriptHashGetHistory(convert(params)?),
            "blockchain.scripthash.get_history_filter" => Params::ScriptHashGetHistoryFilter(convert(params)?),
            "blockchain.scripthash.listunspent" => Params::ScriptHashListUnspent(convert(params)?),
            "blockchain.scripthash.unspent_exist" => Params::ScriptHashUnspentExist(convert(params)?),
            "blockchain.scripthash.select_unspent" => Params::ScriptHashSelectUnspent(convert(params)?),
            "blockchain.scripthash.subscribe" => Params::ScriptHashSubscribe(convert(params)?),
            "blockchain.scripthash.unsubscribe" => Params::ScriptHashUnsubscribe(convert(params)?),
            "blockchain.transaction.broadcast" => Params::TransactionBroadcast(convert(params)?),
            "blockchain.transaction.get" => Params::TransactionGet(convert(params)?),
            "blockchain.transaction.get_merkle" => Params::TransactionGetMerkle(convert(params)?),
            "blockchain.transaction.id_from_pos" => {
                Params::TransactionFromPosition(convert(params)?)
            }
            "mempool.get_fee_histogram" => Params::MempoolFeeHistogram,
            "server.banner" => Params::Banner,
            "server.donation_address" => Params::Donation,
            "server.features" => Params::Features,
            "server.peers.subscribe" => Params::PeersSubscribe,
            "server.ping" => Params::Ping,
            "server.version" => Params::Version(convert(params)?),
            _ => {
                warn!("unknown method {}", method);
                return Err(StandardError::MethodNotFound);
            }
        })
    }
}

struct Call {
    id: Value,
    method: String,
    params: Params,
}

impl Call {
    fn parse(request: Request) -> Result<Call, Value> {
        match Params::parse(&request.method, request.params) {
            Ok(params) => Ok(Call {
                id: request.id,
                method: request.method,
                params,
            }),
            Err(e) => Err(error_msg(&request.id, RpcError::Standard(e))),
        }
    }

    fn response(&self, result: Result<Value>) -> Value {
        match result {
            Ok(value) => result_msg(&self.id, value),
            Err(err) => {
                warn!("RPC {} failed: {:#}", self.method, err);
                match err
                    .downcast_ref::<bitcoincore_rpc::Error>()
                    .and_then(extract_bitcoind_error)
                {
                    Some(e) => error_msg(&self.id, RpcError::DaemonError(e.clone())),
                    None => error_msg(&self.id, RpcError::BadRequest(err)),
                }
            }
        }
    }
}

enum Calls {
    Batch(Vec<Result<Call, Value>>),
    Single(Result<Call, Value>),
}

impl Calls {
    fn parse(requests: Requests) -> Calls {
        match requests {
            Requests::Single(request) => Calls::Single(Call::parse(request)),
            Requests::Batch(batch) => {
                Calls::Batch(batch.into_iter().map(Call::parse).collect::<Vec<_>>())
            }
        }
    }
}

fn convert<T>(params: Value) -> std::result::Result<T, StandardError>
where
    T: serde::de::DeserializeOwned,
{
    let params_str = params.to_string();
    serde_json::from_value(params).map_err(|err| {
        warn!("invalid params {}: {}", params_str, err);
        StandardError::InvalidParams
    })
}

fn notification(method: &str, params: &[Value]) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn result_msg(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_msg(id: &Value, error: RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": error.to_value()})
}

fn error_msg_no_id(err: StandardError) -> Value {
    error_msg(&Value::Null, RpcError::Standard(err))
}

fn parse_requests(line: &str) -> Result<Requests, StandardError> {
    match serde_json::from_str(line) {
        // parse JSON from str
        Ok(value) => match serde_json::from_value(value) {
            // parse RPC from JSON
            Ok(requests) => Ok(requests),
            Err(err) => {
                warn!("invalid RPC request ({:?}): {}", line, err);
                Err(StandardError::InvalidRequest)
            }
        },
        Err(err) => {
            warn!("invalid JSON ({:?}): {}", line, err);
            Err(StandardError::ParseError)
        }
    }
}

fn select_utxos(
    utxos: &[UnspentEntry],
    target_value: Amount,
) -> (Vec<UnspentEntry>, Vec<usize>) {
    let mut choose_list = Vec::new();
    let mut choose_index = Vec::new();
    if utxos.len() <= 3 {
        for (index, utxo) in utxos.iter().enumerate() {
            choose_list.push(utxo.clone());
            choose_index.push(index);
        }
        return (choose_list, choose_index);
    } else {
        let utxo_len = utxos.len();
        if let Some((index, _middle_utxo)) = utxos
            .iter()
            .enumerate()
            .find(|utxo| utxo.1.value >= target_value)
        {
            let select_index = if index == 0 {
                // first utxo is enough
                vec![0, 1, 2]
            } else if index == utxo_len - 1 {
                // last utxo is enough
                vec![0, utxo_len - 2, utxo_len - 1]
            } else {
                // find one middle utxo enough
                if let Some((new_index, _new_utxo)) = utxos[index + 1..]
                    .iter()
                    .enumerate()
                    .find(|utxo| utxo.1.height > 0)
                {
                    // find another confirmed big utxo
                    vec![0, index, new_index + index + 1]
                } else {
                    // no confirmed bit utxo, select last one
                    vec![0, index, utxo_len - 1]
                }
            };
            for i in select_index {
                choose_list.push(utxos[i].clone());
                choose_index.push(i);
            }
        } else {
            let mut total_amount = Amount::from_sat(0);
            let max_len = std::cmp::min(utxo_len, 20);
            // max inputs length is '20'
            for i in 0..=max_len {
                total_amount += utxos[utxo_len - 1 - i].value;
                choose_index.push(utxo_len - 1 - i);
                choose_list.push(utxos[utxo_len - 1 - i].clone());
                if total_amount > target_value {
                    break;
                }
            }
            // push small utxo to make inputs length to 3
            if choose_list.len() < 3 {
                choose_list.push(utxos[0].clone());
                choose_index.push(0);
            }
        };
    }
    (choose_list, choose_index)
}


