use crate::{error, storage::LedgerStorage};
use bip39_dict::Entropy;
use many::server::module::abci_backend::{
    AbciBlock, AbciCommitInfo, AbciInfo, AbciInit, EndpointInfo, ManyAbciModuleBackend,
};
use many::server::module::idstore::{
    GetFromAddressArgs, GetFromRecallPhraseArgs, GetReturns, IdStoreModuleBackend, StoreArgs,
    StoreReturns,
};
use many::server::module::{idstore, ledger};
use many::types::ledger::{Symbol, TokenAmount, Transaction, TransactionKind};
use many::types::{CborRange, Timestamp, VecOrSingle};
use many::{Identity, ManyError};
use minicbor::decode;
use retry::delay::Fixed;
use retry::{retry_with_index, OperationResult};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use tracing::info;

#[cfg(not(test))]
use rand::{thread_rng, Rng};

const MAXIMUM_TRANSACTION_COUNT: usize = 100;

type TxResult = Result<Transaction, ManyError>;

fn filter_account<'a>(
    it: Box<dyn Iterator<Item = TxResult> + 'a>,
    account: Option<VecOrSingle<Identity>>,
) -> Box<dyn Iterator<Item = TxResult> + 'a> {
    if let Some(account) = account {
        let account: Vec<Identity> = account.into();
        Box::new(it.filter(move |t| match t {
            // Propagate the errors.
            Err(_) => true,
            Ok(t) => account.iter().any(|id| t.is_about(id)),
        }))
    } else {
        it
    }
}

fn filter_transaction_kind<'a>(
    it: Box<dyn Iterator<Item = TxResult> + 'a>,
    transaction_kind: Option<VecOrSingle<TransactionKind>>,
) -> Box<dyn Iterator<Item = TxResult> + 'a> {
    if let Some(k) = transaction_kind {
        let k: Vec<TransactionKind> = k.into();
        Box::new(it.filter(move |t| match t {
            Err(_) => true,
            Ok(t) => k.contains(&t.kind()),
        }))
    } else {
        it
    }
}

fn filter_symbol<'a>(
    it: Box<dyn Iterator<Item = TxResult> + 'a>,
    symbol: Option<VecOrSingle<String>>,
) -> Box<dyn Iterator<Item = TxResult> + 'a> {
    if let Some(s) = symbol {
        let s: Vec<String> = s.into();
        Box::new(it.filter(move |t| match t {
            // Propagate the errors.
            Err(_) => true,
            Ok(t) => s.contains(t.symbol()),
        }))
    } else {
        it
    }
}

fn filter_date<'a>(
    it: Box<dyn Iterator<Item = TxResult> + 'a>,
    range: CborRange<Timestamp>,
) -> Box<dyn Iterator<Item = TxResult> + 'a> {
    Box::new(it.filter(move |t| match t {
        // Propagate the errors.
        Err(_) => true,
        Ok(Transaction { time, .. }) => range.contains(time),
    }))
}

/// The initial state schema, loaded from JSON.
#[derive(serde::Deserialize, Debug, Default)]
pub struct InitialStateJson {
    initial: BTreeMap<Identity, BTreeMap<Symbol, TokenAmount>>,
    symbols: BTreeMap<Identity, String>,
    hash: Option<String>,
}

/// A simple ledger that keeps transactions in memory.
#[derive(Debug)]
pub struct LedgerModuleImpl {
    storage: LedgerStorage,
}

impl LedgerModuleImpl {
    pub fn new<P: AsRef<Path>>(
        initial_state: Option<InitialStateJson>,
        persistence_store_path: P,
        // idstore_path: P,
        blockchain: bool,
    ) -> Result<Self, ManyError> {
        let storage = if let Some(state) = initial_state {
            let storage = LedgerStorage::new(
                state.symbols,
                state.initial,
                persistence_store_path,
                blockchain,
            )
            .map_err(ManyError::unknown)?;

            if let Some(h) = state.hash {
                // Verify the hash.
                let actual = hex::encode(storage.hash());
                if actual != h {
                    return Err(error::invalid_initial_state(h, actual));
                }
            }

            storage
        } else {
            LedgerStorage::load(persistence_store_path, blockchain).unwrap()
        };

        info!(
            height = storage.get_height(),
            hash = hex::encode(storage.hash()).as_str()
        );

        Ok(Self { storage })
    }
}

impl ledger::LedgerModuleBackend for LedgerModuleImpl {
    fn info(
        &self,
        _sender: &Identity,
        _args: ledger::InfoArgs,
    ) -> Result<ledger::InfoReturns, ManyError> {
        let storage = &self.storage;

        // Hash the storage.
        let hash = storage.hash();
        let symbols = storage.get_symbols();

        info!(
            "info(): hash={} symbols={:?}",
            hex::encode(storage.hash()).as_str(),
            symbols
        );

        Ok(ledger::InfoReturns {
            symbols: symbols.keys().copied().collect(),
            hash: hash.into(),
            local_names: symbols,
        })
    }

    fn balance(
        &self,
        sender: &Identity,
        args: ledger::BalanceArgs,
    ) -> Result<ledger::BalanceReturns, ManyError> {
        let ledger::BalanceArgs { account, symbols } = args;

        let identity = account.as_ref().unwrap_or(sender);

        let storage = &self.storage;
        let symbols = symbols.unwrap_or_default().0;

        let balances = storage
            .get_multiple_balances(identity, &BTreeSet::from_iter(symbols.clone().into_iter()));
        info!("balance({}, {:?}): {:?}", identity, &symbols, &balances);
        Ok(ledger::BalanceReturns {
            balances: balances.into_iter().map(|(k, v)| (*k, v)).collect(),
        })
    }
}

impl ledger::LedgerCommandsModuleBackend for LedgerModuleImpl {
    fn send(&mut self, sender: &Identity, args: ledger::SendArgs) -> Result<(), ManyError> {
        let ledger::SendArgs {
            from,
            to,
            amount,
            symbol,
        } = args;

        let from = from.as_ref().unwrap_or(sender);

        // TODO: allow some ACLs or delegation on the ledger.
        if from != sender {
            return Err(error::unauthorized());
        }

        self.storage.send(from, &to, &symbol, amount)?;
        Ok(())
    }
}

impl ledger::LedgerTransactionsModuleBackend for LedgerModuleImpl {
    fn transactions(
        &self,
        _args: ledger::TransactionsArgs,
    ) -> Result<ledger::TransactionsReturns, ManyError> {
        Ok(ledger::TransactionsReturns {
            nb_transactions: self.storage.nb_transactions(),
        })
    }

    fn list(&self, args: ledger::ListArgs) -> Result<ledger::ListReturns, ManyError> {
        let ledger::ListArgs {
            count,
            order,
            filter,
        } = args;
        let filter = filter.unwrap_or_default();

        let count = count.map_or(MAXIMUM_TRANSACTION_COUNT, |c| {
            std::cmp::min(c as usize, MAXIMUM_TRANSACTION_COUNT)
        });

        let storage = &self.storage;
        let nb_transactions = storage.nb_transactions();
        let iter = storage.iter(
            filter.id_range.unwrap_or_default(),
            order.unwrap_or_default(),
        );

        let iter = Box::new(iter.map(|(_k, v)| {
            decode::<Transaction>(v.as_slice())
                .map_err(|e| ManyError::deserialization_error(e.to_string()))
        }));

        let iter = filter_account(iter, filter.account);
        let iter = filter_transaction_kind(iter, filter.kind);
        let iter = filter_symbol(iter, filter.symbol);
        let iter = filter_date(iter, filter.date_range.unwrap_or_default());

        let transactions: Vec<Transaction> = iter.take(count).collect::<Result<_, _>>()?;

        Ok(ledger::ListReturns {
            nb_transactions,
            transactions,
        })
    }
}

// This module is always supported, but will only be added when created using an ABCI
// flag.
impl ManyAbciModuleBackend for LedgerModuleImpl {
    #[rustfmt::skip]
    fn init(&mut self) -> Result<AbciInit, ManyError> {
        Ok(AbciInit {
            endpoints: BTreeMap::from([
                ("ledger.info".to_string(), EndpointInfo { is_command: false }),
                ("ledger.balance".to_string(), EndpointInfo { is_command: false }),
                ("ledger.send".to_string(), EndpointInfo { is_command: true }),
                ("ledger.transactions".to_string(), EndpointInfo { is_command: false }),
                ("ledger.list".to_string(), EndpointInfo { is_command: false }),
                ("idstore.store".to_string(), EndpointInfo { is_command: true}),
                ("idstore.getFromRecallPhrase".to_string(), EndpointInfo { is_command: true}),
                ("idstore.getFromAddress".to_string(), EndpointInfo { is_command: true}),
            ]),
        })
    }

    fn init_chain(&mut self) -> Result<(), ManyError> {
        info!("abci.init_chain()",);
        Ok(())
    }

    fn begin_block(&mut self, info: AbciBlock) -> Result<(), ManyError> {
        let time = info.time;
        info!("abci.block_begin(): time={:?}", time);

        if let Some(time) = time {
            let time = UNIX_EPOCH.checked_add(Duration::from_secs(time)).unwrap();
            self.storage.set_time(time);
        }

        Ok(())
    }

    fn info(&self) -> Result<AbciInfo, ManyError> {
        let storage = &self.storage;

        info!(
            "abci.info(): height={} hash={}",
            storage.get_height(),
            hex::encode(storage.hash()).as_str()
        );
        Ok(AbciInfo {
            height: storage.get_height(),
            hash: storage.hash().into(),
        })
    }

    fn commit(&mut self) -> Result<AbciCommitInfo, ManyError> {
        let result = self.storage.commit();

        info!(
            "abci.commit(): retain_height={} hash={}",
            result.retain_height,
            hex::encode(result.hash.as_slice()).as_str()
        );
        Ok(result)
    }
}

#[cfg(not(test))]
fn generate_entropy<const FB: usize>() -> Entropy<FB> {
    let mut random = [0u8; FB];
    thread_rng().fill(&mut random[..]);
    bip39_dict::Entropy::<FB>(random)
}

#[cfg(test)]
pub fn generate_entropy<const FB: usize>() -> Entropy<FB> {
    bip39_dict::Entropy::<FB>::generate(|| 1)
}

pub fn generate_recall_phrase<const W: usize, const FB: usize, const CS: usize>() -> Vec<String> {
    let mnemonic = generate_entropy::<FB>().to_mnemonics::<W, CS>().unwrap();
    let recall_phrase = mnemonic
        .to_string(&bip39_dict::ENGLISH)
        .split_whitespace()
        .map(|e| e.to_string()) // TODO: This is ugly
        .collect::<Vec<String>>();
    recall_phrase
}

impl IdStoreModuleBackend for LedgerModuleImpl {
    fn store(
        &mut self,
        StoreArgs { address, cred_id }: StoreArgs,
    ) -> Result<StoreReturns, ManyError> {
        if !address.is_public_key() {
            return Err(idstore::invalid_address(address.to_string()));
        }

        if !(16..=1023).contains(&cred_id.0.len()) {
            return Err(idstore::invalid_credential_id(hex::encode(&*cred_id.0)));
        }

        let recall_phrase = retry_with_index(Fixed::from_millis(10), |current_try| {
            if current_try > 8 {
                return OperationResult::Err(
                    "Unable to create recall phrase after 8 try. Aborting.",
                );
            }

            let recall_phrase = match current_try {
                1 | 2 => generate_recall_phrase::<2, 2, 6>(),
                3 | 4 => generate_recall_phrase::<3, 4, 1>(),
                5 | 6 => generate_recall_phrase::<4, 5, 4>(),
                7 | 8 => generate_recall_phrase::<5, 6, 7>(),
                _ => {
                    unimplemented!()
                }
            };

            if self.storage.get_from_recall_phrase(&recall_phrase).is_ok() {
                OperationResult::Retry("Recall phrase generation failed, retrying...")
            } else {
                OperationResult::Ok(recall_phrase)
            }
        })
        .map_err(|_| idstore::recall_phrase_generation_failed())?;

        self.storage.store(&recall_phrase, address, cred_id)?;
        Ok(StoreReturns(recall_phrase))
    }

    fn get_from_recall_phrase(
        &self,
        args: GetFromRecallPhraseArgs,
    ) -> Result<GetReturns, ManyError> {
        Ok(GetReturns(self.storage.get_from_recall_phrase(&args.0)?))
    }

    fn get_from_address(&self, args: GetFromAddressArgs) -> Result<GetReturns, ManyError> {
        Ok(GetReturns(self.storage.get_from_address(args.0)?))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use many::server::module::idstore::CredentialId;
    use minicbor::bytes::ByteVec;

    use super::*;

    fn setup() -> (Identity, CredentialId, tempfile::TempDir) {
        let address =
            Identity::from_str("maffbahksdwaqeenayy2gxke32hgb7aq4ao4wt745lsfs6wijp").unwrap();
        let cred_id = CredentialId(ByteVec::from(Vec::from([1; 16])));
        let persistent = tempfile::tempdir().unwrap();

        (address, cred_id, persistent)
    }

    #[test]
    fn idstore_store() {
        let (address, cred_id, persistent) = setup();
        let mut module_impl = LedgerModuleImpl::new(None, persistent, false).unwrap();

        // Try storing the same credential until we reach 5 words
        for i in 2..=5 {
            let result = module_impl.store(StoreArgs {
                address,
                cred_id: cred_id.clone(),
            });
            assert!(result.is_ok());
            let recall_phrase = result.unwrap().0;
            assert_eq!(recall_phrase.len(), i);
        }

        // Make sure we abort after reaching 5 words
        let result = module_impl.store(StoreArgs { address, cred_id });
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            idstore::recall_phrase_generation_failed().code
        );
    }

    #[test]
    fn idstore_invalid_cred_id() {
        let (address, _, persistent) = setup();
        let mut module_impl = LedgerModuleImpl::new(None, persistent, false).unwrap();

        let cred_id = CredentialId(ByteVec::from(Vec::from([1; 15])));
        let result = module_impl.store(StoreArgs {
            address,
            cred_id,
        });
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            idstore::invalid_credential_id("".to_string()).code
        );

        let cred_id = CredentialId(ByteVec::from(Vec::from([1; 1024])));
        let result = module_impl.store(StoreArgs { address, cred_id });
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            idstore::invalid_credential_id("".to_string()).code
        );
    }

    #[test]
    fn idstore_get_from_recall_phrase() {
        let (address, cred_id, persistent) = setup();
        let mut module_impl = LedgerModuleImpl::new(None, persistent, false).unwrap();
        let result = module_impl.store(StoreArgs {
            address,
            cred_id: cred_id.clone(),
        });

        assert!(result.is_ok());
        let store_return = result.unwrap();

        let result = module_impl.get_from_recall_phrase(GetFromRecallPhraseArgs(store_return.0));
        assert!(result.is_ok());
        let get_returns = result.unwrap();

        assert_eq!(get_returns.0, cred_id);
    }

    #[test]
    fn idstore_get_from_address() {
        let (address, cred_id, persistent) = setup();
        let mut module_impl = LedgerModuleImpl::new(None, persistent, false).unwrap();
        let result = module_impl.store(StoreArgs {
            address,
            cred_id: cred_id.clone(),
        });

        assert!(result.is_ok());

        let result = module_impl.get_from_address(GetFromAddressArgs(address));
        assert!(result.is_ok());
        let get_returns = result.unwrap();

        assert_eq!(get_returns.0, cred_id);
    }
}
