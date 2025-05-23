// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/
use cfx_types::H256;
use cfxcore::{
    consensus::pos_handler::save_initial_nodes_to_file,
    genesis_block::{
        register_transaction, GenesisPosNodeInfo, GenesisPosState,
    },
};
use cfxkey::{Error as EthkeyError, Generator, KeyPair, Random};
use clap4;
use diem_crypto::{
    key_file::save_pri_key, HashValue, Uniform, ValidCryptoMaterialStringExt,
};
use diem_types::{
    account_address::AccountAddress,
    contract_event::ContractEvent,
    on_chain_config::{new_epoch_event_key, ValidatorSet},
    term_state::{
        pos_state_config::{PosStateConfigTrait, POS_STATE_CONFIG},
        NodeID, TERM_LIST_LEN,
    },
    transaction::{ChangeSet, Transaction, WriteSetPayload},
    validator_config::{
        ConsensusPrivateKey, ConsensusPublicKey, ConsensusVRFPrivateKey,
        ConsensusVRFPublicKey, ValidatorConfig,
    },
    validator_info::ValidatorInfo,
    waypoint::Waypoint,
    write_set::WriteSet,
};
use executor::{db_bootstrapper::generate_waypoint, vm::PosVM};
use pos_ledger_db::PosLedgerDB;
use rand_08::{rngs::StdRng, SeedableRng};
use rustc_hex::FromHexError;
use std::{
    collections::{BTreeMap, BinaryHeap, HashMap},
    fmt,
    fs::File,
    io::{self, Read, Write},
    num::ParseIntError,
    path::{Path, PathBuf},
    process,
    result::Result,
};
use storage_interface::DbReaderWriter;
use tempfile::Builder;

#[derive(Debug)]
enum Error {
    Ethkey(EthkeyError),
    FromHex(FromHexError),
    ParseInt(ParseIntError),
    Io(io::Error),
    Fmt(fmt::Error),
}

impl From<EthkeyError> for Error {
    fn from(err: EthkeyError) -> Self { Error::Ethkey(err) }
}

impl From<FromHexError> for Error {
    fn from(err: FromHexError) -> Self { Error::FromHex(err) }
}

impl From<ParseIntError> for Error {
    fn from(err: ParseIntError) -> Self { Error::ParseInt(err) }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self { Error::Io(err) }
}

impl From<std::fmt::Error> for Error {
    fn from(err: std::fmt::Error) -> Self { Error::Fmt(err) }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            Error::Ethkey(ref e) => write!(f, "{}", e),
            Error::FromHex(ref e) => write!(f, "{}", e),
            Error::ParseInt(ref e) => write!(f, "{}", e),
            Error::Io(ref e) => write!(f, "{}", e),
            Error::Fmt(ref e) => write!(f, "{}", e),
        }
    }
}

fn main() {
    env_logger::try_init().expect("Logger initialized only once.");
    POS_STATE_CONFIG.set(Default::default()).unwrap();

    let cli = clap4::Command::new("pos-genesis-tool")
        .subcommand_required(true)
        .subcommands([
            clap4::Command::new("random")
                .arg(
                    clap4::Arg::new("initial-seed")
                        .long("initial-seed")
                        .required(true),
                )
                .arg(
                    clap4::Arg::new("num-validator")
                        .long("num-validator")
                        .value_parser(clap4::value_parser!(usize)),
                )
                .arg(
                    clap4::Arg::new("num-genesis-validator")
                        .long("num-genesis-validator")
                        .value_parser(clap4::value_parser!(usize)),
                )
                .arg(
                    clap4::Arg::new("chain-id")
                        .long("chain-id")
                        .value_parser(clap4::value_parser!(u32)),
                ),
            clap4::Command::new("frompub")
                .arg(
                    clap4::Arg::new("initial-seed")
                        .long("initial-seed")
                        .required(true),
                )
                .arg(
                    clap4::Arg::new("pkfile")
                        .value_parser(clap4::value_parser!(PathBuf))
                        .required(true),
                ),
        ]);

    match execute(cli) {
        Ok(ok) => println!("{}", ok),
        Err(err) => {
            eprintln!("{}", err);
            process::exit(1);
        }
    }
}

fn execute_genesis_transaction(genesis_txn: Transaction) -> Waypoint {
    let tmp_dir = Builder::new().prefix("example").tempdir().unwrap();
    let (_, db) = DbReaderWriter::wrap(
        PosLedgerDB::open(
            tmp_dir.path(),
            false, /* readonly */
            Some(1_000_000),
            Default::default(),
        )
        .expect("DB should open."),
    );
    generate_waypoint::<PosVM>(&db, &genesis_txn).unwrap()
}

fn generate_genesis_from_public_keys(public_keys: Vec<(NodeID, u64)>) {
    let genesis_path = PathBuf::from("./genesis_file");
    let waypoint_path = PathBuf::from("./waypoint_config");
    let mut genesis_file = File::create(&genesis_path).unwrap();
    let mut waypoint_file = File::create(&waypoint_path).unwrap();

    let mut validators = Vec::new();
    for (node_id, voting_power) in public_keys {
        let validator_config = ValidatorConfig::new(
            node_id.public_key,
            Some(node_id.vrf_public_key),
            vec![],
            vec![],
        );
        validators.push(ValidatorInfo::new(
            node_id.addr,
            voting_power,
            validator_config,
        ));
    }
    let validator_set = ValidatorSet::new(validators);
    let validator_set_bytes = bcs::to_bytes(&validator_set).unwrap();
    let contract_event =
        ContractEvent::new(new_epoch_event_key(), validator_set_bytes);
    let change_set = ChangeSet::new(WriteSet::default(), vec![contract_event]);
    let write_set_paylod = WriteSetPayload::Direct(change_set);
    let genesis_transaction = Transaction::GenesisTransaction(write_set_paylod);
    let genesis_bytes = bcs::to_bytes(&genesis_transaction).unwrap();
    genesis_file.write_all(&genesis_bytes).unwrap();

    let waypoint = execute_genesis_transaction(genesis_transaction);
    waypoint_file
        .write_all(waypoint.to_string().as_bytes())
        .unwrap();
}

fn elect_genesis_committee(
    initial_nodes: &Vec<GenesisPosNodeInfo>, initial_seed: &[u8],
) -> Vec<(NodeID, u64)> {
    let mut node_map = HashMap::new();
    let mut electing_heap = BinaryHeap::new();
    for node in initial_nodes {
        let node_id = NodeID::new(node.bls_key.clone(), node.vrf_key.clone());
        let buffer = [node_id.addr.as_ref(), initial_seed].concat();
        for nonce in 0..node.voting_power {
            electing_heap.push((
                HashValue::sha3_256_of(
                    &[&buffer as &[u8], &nonce.to_be_bytes()].concat(),
                ),
                node_id.addr,
            ));
        }
        node_map.insert(node_id.addr, node_id);
    }
    let max_committee_size =
        TERM_LIST_LEN * POS_STATE_CONFIG.term_elected_size();
    let mut top_electing: BTreeMap<AccountAddress, u64> = BTreeMap::new();
    let mut count = 0usize;
    while let Some((_, node_id)) = electing_heap.pop() {
        *top_electing.entry(node_id).or_insert(0) += 1;
        count += 1;
        if count >= max_committee_size {
            break;
        }
    }
    let mut elected = Vec::with_capacity(top_electing.len());
    for (addr, voting_power) in top_electing {
        elected.push((node_map.remove(&addr).unwrap(), voting_power));
    }
    elected
}

fn execute(command: clap4::Command) -> Result<String, Error> {
    let matches = command.get_matches();
    return match matches.subcommand() {
        Some(("random", sub_matches)) => {
            let initial_seed: H256 = sub_matches
                .get_one::<String>("initial-seed")
                .expect("initial-seed is required")
                .clone()
                .parse()
                .expect("invalid initial seed");
            let num_validator = sub_matches
                .get_one::<usize>("num-validator")
                .unwrap_or(&1)
                .clone();
            let num_genesis_validator = sub_matches
                .get_one::<usize>("num-genesis-validator")
                .unwrap_or(&1)
                .clone();
            let chain_id =
                sub_matches.get_one::<u32>("chain-id").unwrap_or(&0).clone();

            if num_genesis_validator > num_validator {
                panic!("The number of genesis validators cannot be more than the total number of \
            validators: {} > {}", num_genesis_validator, num_validator);
            }

            let private_key_dir = PathBuf::from("./private_keys");
            std::fs::create_dir(&private_key_dir)?;
            let public_key_path = PathBuf::from("./public_key");
            let mut public_key_file = File::create(&public_key_path)?;
            let mut rng = StdRng::from_seed([0u8; 32]);
            let mut genesis_nodes = Vec::new();

            let voting_power = 2_000;
            for i in 0..num_validator {
                let pow_keypair: KeyPair = Random.generate().unwrap();
                let private_key = ConsensusPrivateKey::generate(&mut rng);
                let vrf_private_key =
                    ConsensusVRFPrivateKey::generate(&mut rng);
                save_pri_key(
                    private_key_dir.join(PathBuf::from(i.to_string())),
                    &[],
                    &(&private_key, &vrf_private_key),
                )
                .expect("Error saving private keys");
                File::create(
                    private_key_dir.join(Path::new(&format!("pow_sk{}", i))),
                )?
                .write_all(pow_keypair.secret().as_bytes())?;

                let public_key = ConsensusPublicKey::from(&private_key);
                let vrf_public_key =
                    ConsensusVRFPublicKey::from(&vrf_private_key);
                let register_tx = register_transaction(
                    private_key,
                    vrf_public_key.clone(),
                    voting_power,
                    chain_id,
                    false,
                );
                let public_key_str = public_key.to_encoded_string().unwrap();
                let vrf_public_key_str =
                    vrf_public_key.to_encoded_string().unwrap();
                let public_key_str = format!(
                    "{},{},{}\n",
                    public_key_str, vrf_public_key_str, voting_power
                );
                public_key_file.write_all(public_key_str.as_bytes())?;
                genesis_nodes.push(GenesisPosNodeInfo {
                    address: pow_keypair.address(),
                    bls_key: public_key,
                    vrf_key: vrf_public_key,
                    voting_power,
                    register_tx,
                });
            }
            let initial_nodes = genesis_nodes[..num_genesis_validator].to_vec();
            let initial_committee = elect_genesis_committee(
                &initial_nodes,
                initial_seed.as_bytes(),
            );
            save_initial_nodes_to_file(
                "./initial_nodes.json",
                GenesisPosState {
                    initial_nodes,
                    initial_committee: initial_committee
                        .iter()
                        .map(|(node_id, voting_power)| {
                            (node_id.addr, *voting_power)
                        })
                        .collect(),
                    initial_seed,
                },
            );
            generate_genesis_from_public_keys(initial_committee);
            Ok("Ok".into())
        }

        Some(("frompub", sub_matches)) => {
            let initial_seed: H256 = sub_matches
                .get_one::<String>("initial-seed")
                .expect("initial-seed is required")
                .clone()
                .parse()
                .expect("invalid initial seed");
            let public_key_path = sub_matches
                .get_one::<PathBuf>("pkfile")
                .expect("pkfile is required")
                .clone();

            let mut public_key_file = File::open(&public_key_path)?;
            let mut contents = String::new();
            public_key_file.read_to_string(&mut contents)?;
            let mut lines = contents.as_str().lines();

            let mut public_keys = Vec::new();
            let mut genesis_nodes = Vec::new();
            while let Some(key_str) = lines.next() {
                let key_array: Vec<_> = key_str.split(",").collect();
                let public_key =
                    ConsensusPublicKey::from_encoded_string(key_array[0])
                        .unwrap();
                let vrf_public_key =
                    ConsensusVRFPublicKey::from_encoded_string(key_array[1])
                        .unwrap();
                let voting_power: u64 = key_array[2].parse().unwrap();
                public_keys.push((
                    public_key.clone(),
                    vrf_public_key.clone(),
                    voting_power,
                ));
                genesis_nodes.push(GenesisPosNodeInfo {
                    // Not used in PoS genesis.
                    address: Default::default(),
                    bls_key: public_key,
                    vrf_key: vrf_public_key,
                    voting_power,
                    // Not used in PoS genesis.
                    register_tx: Default::default(),
                });
            }
            let initial_committee = elect_genesis_committee(
                &genesis_nodes,
                initial_seed.as_bytes(),
            );
            save_initial_nodes_to_file(
                "./initial_nodes.json",
                GenesisPosState {
                    initial_nodes: genesis_nodes,
                    initial_committee: initial_committee
                        .iter()
                        .map(|(node_id, voting_power)| {
                            (node_id.addr, *voting_power)
                        })
                        .collect(),
                    initial_seed,
                },
            );
            generate_genesis_from_public_keys(initial_committee);
            Ok("Ok".into())
        }

        _ => unreachable!("clap should ensure we don't get here"),
    };
}
