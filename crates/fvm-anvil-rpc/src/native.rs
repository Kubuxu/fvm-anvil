use anvil_rpc::{error::RpcError, response::ResponseResult};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cid::Cid;
use fvm_anvil_node::{HeadChange, NativeTipset, NodeHandle};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct CidJson {
    #[serde(rename = "/")]
    cid: String,
}

fn key(value: &Value) -> Result<Vec<Cid>, String> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let values: Vec<CidJson> = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    values
        .into_iter()
        .map(|v| v.cid.parse().map_err(|e| format!("invalid tipset CID: {e}")))
        .collect()
}

fn cid(value: Cid) -> Value {
    json!({"/": value.to_string()})
}

pub(crate) fn tipset(tipset: NativeTipset) -> Value {
    let parents: Vec<_> = tipset.parents.into_iter().map(cid).collect();
    json!({
        "Cids": [cid(tipset.cid)],
        "Height": tipset.height,
        "Blocks": [{
            "Miner": "f00",
            "Ticket": {"VRFProof": STANDARD.encode(tipset.ticket)},
            "ElectionProof": null,
            "BeaconEntries": [{"Round": tipset.height, "Data": STANDARD.encode(tipset.beacon)}],
            "WinPoStProof": [],
            "Parents": parents,
            "ParentWeight": tipset.height.to_string(),
            "Height": tipset.height,
            "ParentStateRoot": cid(tipset.parent_state_root),
            "ParentMessageReceipts": cid(tipset.parent_message_receipts),
            "Messages": cid(tipset.messages),
            "BLSAggregate": null,
            "Timestamp": tipset.timestamp,
            "BlockSig": null,
            "ForkSignaling": 0,
            "ParentBaseFee": tipset.base_fee.to_string()
        }]
    })
}

pub(crate) fn changes(changes: Vec<HeadChange>) -> Value {
    Value::Array(
        changes
            .into_iter()
            .map(|change| {
                let (kind, head) = match change {
                    HeadChange::Current(head) => ("current", head),
                    HeadChange::Apply(head) => ("apply", head),
                    HeadChange::Revert(head) => ("revert", head),
                };
                json!({"Type": kind, "Val": tipset(head)})
            })
            .collect(),
    )
}

pub(crate) async fn dispatch(node: &NodeHandle, method: &str, params: Value) -> ResponseResult {
    let result = execute(node, method, params).await;
    match result {
        Ok(value) => ResponseResult::success(value),
        Err(error) => ResponseResult::error(error),
    }
}

async fn execute(node: &NodeHandle, method: &str, params: Value) -> Result<Value, RpcError> {
    let params = match params {
        Value::Null => Vec::new(),
        Value::Array(params) => params,
        _ => return Err(RpcError::invalid_params("expected positional parameters")),
    };
    let expected = match method {
        "Filecoin.StateNetworkName" | "Filecoin.ChainHead" => 0,
        "Filecoin.StateNetworkVersion" | "Filecoin.ChainGetTipSet" => 1,
        "Filecoin.StateGetRandomnessDigestFromBeacon" | "Filecoin.ChainGetTipSetByHeight" => 2,
        "Filecoin.ChainNotify" => {
            return Err(RpcError::invalid_params("ChainNotify requires WebSocket transport"));
        }
        _ => return Err(RpcError::method_not_found()),
    };
    if params.len() != expected {
        return Err(RpcError::invalid_params(format!("expected {expected} parameters")));
    }
    let node_error = |error: fvm_anvil_node::NodeError| RpcError::invalid_params(error.to_string());
    let parse_key = |value: &Value| key(value).map_err(RpcError::invalid_params);
    match method {
        "Filecoin.StateNetworkName" => Ok(json!("local-fvm-anvil")),
        "Filecoin.ChainHead" => {
            Ok(tipset(node.native_tipset(Vec::new()).await.map_err(node_error)?))
        }
        "Filecoin.StateNetworkVersion" => {
            node.native_tipset(parse_key(&params[0])?).await.map_err(node_error)?;
            Ok(json!(u32::from(node.network_version())))
        }
        "Filecoin.ChainGetTipSet" => {
            Ok(tipset(node.native_tipset(parse_key(&params[0])?).await.map_err(node_error)?))
        }
        "Filecoin.ChainGetTipSetByHeight" => {
            let height = params[0]
                .as_i64()
                .ok_or_else(|| RpcError::invalid_params("height must be an int64"))?;
            Ok(tipset(
                node.native_tipset_by_height(height, parse_key(&params[1])?)
                    .await
                    .map_err(node_error)?,
            ))
        }
        "Filecoin.StateGetRandomnessDigestFromBeacon" => {
            let epoch = params[0]
                .as_i64()
                .ok_or_else(|| RpcError::invalid_params("epoch must be an int64"))?;
            let digest =
                node.beacon_digest(epoch, parse_key(&params[1])?).await.map_err(node_error)?;
            Ok(json!(STANDARD.encode(digest)))
        }
        _ => Err(RpcError::method_not_found()),
    }
}
