// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use crate::rpc::types::{Bytes, RpcAddress};
use cfx_addr::Network;
use cfx_types::{Space, H256, U256};
use primitives::log_entry::{LocalizedLogEntry, LogEntry};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Log {
    /// Address
    pub address: RpcAddress,

    /// Topics
    pub topics: Vec<H256>,

    /// Data
    pub data: Bytes,

    /// Block Hash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<H256>,

    /// Epoch Number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch_number: Option<U256>,

    /// Transaction Hash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_hash: Option<H256>,

    /// Transaction Index
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_index: Option<U256>,

    /// Log Index in Block
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_index: Option<U256>,

    /// Log Index in Transaction
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_log_index: Option<U256>,

    /// Log space
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<Space>,
}

impl Log {
    pub fn try_from_localized(
        e: LocalizedLogEntry, network: Network,
    ) -> Result<Log, String> {
        Ok(Log {
            address: RpcAddress::try_from_h160(e.entry.address, network)?,
            topics: e.entry.topics.into_iter().map(Into::into).collect(),
            data: e.entry.data.into(),
            block_hash: Some(e.block_hash.into()),
            epoch_number: Some(e.epoch_number.into()),
            transaction_hash: Some(e.transaction_hash.into()),
            transaction_index: Some(e.transaction_index.into()),
            log_index: Some(e.log_index.into()),
            transaction_log_index: Some(e.transaction_log_index.into()),
            space: None,
        })
    }

    pub fn try_from(
        e: LogEntry, network: Network, include_space: bool,
    ) -> Result<Log, String> {
        Ok(Log {
            address: RpcAddress::try_from_h160(e.address, network)?,
            topics: e.topics.into_iter().map(Into::into).collect(),
            data: e.data.into(),
            block_hash: None,
            epoch_number: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            transaction_log_index: None,
            space: if include_space { Some(e.space) } else { None },
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::rpc::types::{Log, RpcAddress};
    use cfx_addr::Network;
    use cfx_types::{Space, H160, H256, U256};
    use serde_json;
    use std::str::FromStr;

    #[test]
    fn log_serialization() {
        let s = r#"{"address":"CFXTEST:TYPE.USER:AAK3WAKCPSF3CP0MFHDWHTTUG924VERHBUV9NMM3YC","topics":["0xa6697e974e6a320f454390be03f74955e8978f1a6971ea6730542e37b66179bc","0x4861736852656700000000000000000000000000000000000000000000000000"],"data":"0x","blockHash":"0xed76641c68a1c641aee09a94b3b471f4dc0316efe5ac19cf488e2674cf8d05b5","epochNumber":"0x4510c","transactionHash":"0x0000000000000000000000000000000000000000000000000000000000000000","transactionIndex":"0x0","logIndex":"0x1","transactionLogIndex":"0x1","space":"evm"}"#;

        let log = Log {
            address: RpcAddress::try_from_h160(H160::from_str("13990122638b9132ca29c723bdf037f1a891a70c").unwrap(), Network::Test).unwrap(),
            topics: vec![
                H256::from_str("a6697e974e6a320f454390be03f74955e8978f1a6971ea6730542e37b66179bc").unwrap(),
                H256::from_str("4861736852656700000000000000000000000000000000000000000000000000").unwrap(),
            ],
            data: vec![].into(),
            block_hash: Some(H256::from_str("ed76641c68a1c641aee09a94b3b471f4dc0316efe5ac19cf488e2674cf8d05b5").unwrap()),
            epoch_number: Some(U256::from(0x4510c)),
            transaction_hash: Some(H256::default()),
            transaction_index: Some(U256::default()),
            transaction_log_index: Some(1.into()),
            log_index: Some(U256::from(1)),
            space: Some(Space::Ethereum),
        };

        let serialized = serde_json::to_string(&log).unwrap();
        assert_eq!(serialized, s);
    }
}
