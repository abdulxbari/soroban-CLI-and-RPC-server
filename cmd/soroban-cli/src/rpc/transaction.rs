use soroban_env_host::xdr::{
    ContractAuth, DiagnosticEvent, HostFunction, OperationBody, ReadXdr, SorobanTransactionData,
    Transaction, TransactionExt, VecM,
};

use crate::rpc::{Error, LogEvents, SimulateTransactionResponse};

// Apply the result of a simulateTransaction onto a transaction envelope, preparing it for
// submission to the network.
pub fn assemble(
    raw: &Transaction,
    simulation: &SimulateTransactionResponse,
    log_events: Option<LogEvents>,
) -> Result<Transaction, Error> {
    let mut tx = raw.clone();

    // Right now simulate.results is one-result-per-function, and assumes there is only one
    // operation in the txn, so we need to enforce that here. I (Paul) think that is a bug
    // in soroban-rpc.simulateTransaction design, and we should fix it there.
    // TODO: We should to better handling so non-soroban txns can be a passthrough here.
    if tx.operations.len() != 1 {
        return Err(Error::UnexpectedOperationCount {
            count: tx.operations.len(),
        });
    }

    // TODO: Should we keep this?
    let events = simulation
        .events
        .iter()
        .map(DiagnosticEvent::from_xdr_base64)
        .collect::<Result<Vec<_>, _>>()?;
    if !events.is_empty() {
        tracing::debug!(simulation_events=?events);
    }

    // update the fees of the actual transaction to meet the minimum resource fees.
    let mut fee = tx.fee;
    let classic_transaction_fees = crate::fee::Args::default().fee;
    if fee < classic_transaction_fees + simulation.min_resource_fee {
        fee = classic_transaction_fees + simulation.min_resource_fee;
    }

    let transaction_data = SorobanTransactionData::from_xdr_base64(&simulation.transaction_data)?;

    let mut op = tx.operations[0].clone();
    if let OperationBody::InvokeHostFunction(ref mut body) = &mut op.body {
        if simulation.results.len() != body.functions.len() {
            return Err(Error::UnexpectedSimulateTransactionResultSize {
                length: simulation.results.len(),
            });
        }

        let auths = simulation
            .results
            .iter()
            .map(|r| {
                VecM::try_from(
                    r.auth
                        .iter()
                        .map(ContractAuth::from_xdr_base64)
                        .collect::<Result<Vec<_>, _>>()?,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(log) = log_events {
            log(&transaction_data.resources.footprint, &auths, &[], None);
        }
        body.functions = body
            .functions
            .iter()
            .zip(auths)
            .map(|(f, auth)| HostFunction {
                args: f.args.clone(),
                auth,
            })
            .collect::<Vec<_>>()
            .try_into()?;
    } else {
        return Err(Error::UnsupportedOperationType);
    }

    tx.fee = fee;
    tx.operations = vec![op].try_into()?;
    tx.ext = TransactionExt::V1(transaction_data);
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::{Cost, SimulateHostFunctionResult};
    use soroban_env_host::xdr::{
        AccountId, AddressWithNonce, AuthorizedInvocation, ChangeTrustAsset, ChangeTrustOp,
        ExtensionPoint, Hash, HostFunctionArgs, InvokeHostFunctionOp, LedgerFootprint, Memo,
        MuxedAccount, Operation, Preconditions, PublicKey, ScAddress, ScSymbol, ScVal, ScVec,
        SequenceNumber, SorobanResources, SorobanTransactionData, Uint256, WriteXdr,
    };
    use stellar_strkey::ed25519::PublicKey as Ed25519PublicKey;

    const SOURCE: &str = "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI";

    fn transaction_data() -> SorobanTransactionData {
        SorobanTransactionData {
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: VecM::default(),
                    read_write: VecM::default(),
                },
                instructions: 0,
                read_bytes: 5,
                write_bytes: 0,
                extended_meta_data_size_bytes: 0,
            },
            refundable_fee: 0,
            ext: ExtensionPoint::V0,
        }
    }

    fn simulation_response() -> SimulateTransactionResponse {
        let source_bytes = Ed25519PublicKey::from_string(SOURCE).unwrap().0;
        let fn_auth = &ContractAuth {
            address_with_nonce: Some(AddressWithNonce {
                address: ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    source_bytes,
                )))),
                nonce: 0,
            }),
            root_invocation: AuthorizedInvocation {
                contract_id: Hash([0; 32]),
                function_name: ScSymbol("fn".try_into().unwrap()),
                args: ScVec(VecM::default()),
                sub_invocations: VecM::default(),
            },
            signature_args: ScVec(VecM::default()),
        };

        SimulateTransactionResponse {
            error: None,
            transaction_data: transaction_data().to_xdr_base64().unwrap(),
            events: Vec::default(),
            min_resource_fee: 115,
            results: vec![SimulateHostFunctionResult {
                auth: vec![fn_auth.to_xdr_base64().unwrap()],
                xdr: ScVal::U32(0).to_xdr_base64().unwrap(),
            }],
            cost: Cost {
                cpu_insns: "0".to_string(),
                mem_bytes: "0".to_string(),
            },
            latest_ledger: 3,
        }
    }

    fn single_contract_fn_transaction() -> Transaction {
        let source_bytes = Ed25519PublicKey::from_string(SOURCE).unwrap().0;
        Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(source_bytes)),
            fee: 100,
            seq_num: SequenceNumber(0),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    functions: vec![HostFunction {
                        args: HostFunctionArgs::InvokeContract(ScVec(VecM::default())),
                        auth: VecM::default(),
                    }]
                    .try_into()
                    .unwrap(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        }
    }

    #[test]
    fn test_assemble_transaction_updates_tx_data_from_simulation_response() {
        let sim = simulation_response();
        let txn = single_contract_fn_transaction();
        let Ok(result) = assemble(&txn, &sim, None) else {
            panic!("assemble failed");
        };

        // validate it auto updated the tx fees from sim response fees
        // since it was greater than tx.fee
        assert_eq!(215, result.fee);

        // validate it updated sorobantransactiondata block in the tx ext
        assert_eq!(TransactionExt::V1(transaction_data()), result.ext);
    }

    #[test]
    fn test_assemble_transaction_adds_the_auth_to_the_host_function() {
        let sim = simulation_response();
        let txn = single_contract_fn_transaction();
        let Ok(result) = assemble(&txn, &sim, None) else {
            panic!("assemble failed");
        };

        assert_eq!(1, result.operations.len());
        let OperationBody::InvokeHostFunction(ref op) = result.operations[0].body else {
            panic!("unexpected operation type: {:#?}", result.operations[0]);
        };

        assert_eq!(1, op.functions.len());
        assert_eq!(1, op.functions[0].auth.len());
        let auth = &op.functions[0].auth[0];

        assert_eq!(
            "fn".to_string(),
            format!("{}", auth.root_invocation.function_name.0),
        );

        assert_eq!(
            Some(SOURCE.to_string()),
            auth.address_with_nonce
                .clone()
                .map(|a| a.address)
                .map(|a| match a {
                    ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(u))) => u,
                    _ => panic!("unexpected address type"),
                })
                .map(|k| stellar_strkey::ed25519::PublicKey(k.try_into().unwrap()))
                .map(|p| p.to_string())
        );
    }

    #[test]
    fn test_assemble_transaction_errors_for_non_invokehostfn_ops() {
        let source_bytes = Ed25519PublicKey::from_string(SOURCE).unwrap().0;
        let txn = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(source_bytes)),
            fee: 100,
            seq_num: SequenceNumber(0),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::ChangeTrust(ChangeTrustOp {
                    line: ChangeTrustAsset::Native,
                    limit: 0,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };

        let result = assemble(
            &txn,
            &SimulateTransactionResponse {
                error: None,
                transaction_data: transaction_data().to_xdr_base64().unwrap(),
                events: Vec::default(),
                min_resource_fee: 115,
                results: vec![],
                cost: Cost {
                    cpu_insns: "0".to_string(),
                    mem_bytes: "0".to_string(),
                },
                latest_ledger: 3,
            },
            None,
        );

        match result {
            Err(Error::UnsupportedOperationType) => {}
            r => panic!("expected unsupportOperationType error, got: {r:#?}"),
        }
    }

    #[test]
    fn test_assemble_transaction_errors_for_errors_for_mismatched_simulation() {
        let txn = single_contract_fn_transaction();

        let result = assemble(
            &txn,
            &SimulateTransactionResponse {
                error: None,
                transaction_data: transaction_data().to_xdr_base64().unwrap(),
                events: Vec::default(),
                min_resource_fee: 115,
                results: vec![],
                cost: Cost {
                    cpu_insns: "0".to_string(),
                    mem_bytes: "0".to_string(),
                },
                latest_ledger: 3,
            },
            None,
        );

        match result {
            Err(Error::UnexpectedSimulateTransactionResultSize { length }) => {
                assert_eq!(0, length);
            }
            r => panic!("expected UnexpectedSimulateTransactionResultSize error, got: {r:#?}"),
        }
    }

    #[test]
    fn test_assemble_transaction_handles_no_host_functions() {
        let source_bytes = Ed25519PublicKey::from_string(SOURCE).unwrap().0;
        let txn = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(source_bytes)),
            fee: 100,
            seq_num: SequenceNumber(0),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    // This is empty
                    functions: vec![].try_into().unwrap(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };

        let result = assemble(
            &txn,
            &SimulateTransactionResponse {
                error: None,
                transaction_data: transaction_data().to_xdr_base64().unwrap(),
                events: Vec::default(),
                min_resource_fee: 115,
                results: vec![],
                cost: Cost {
                    cpu_insns: "0".to_string(),
                    mem_bytes: "0".to_string(),
                },
                latest_ledger: 3,
            },
            None,
        );

        match result {
            Ok(Transaction { operations, .. }) => {
                assert_eq!(1, operations.len());
                match operations[0].body {
                    OperationBody::InvokeHostFunction(ref op) => {
                        assert_eq!(0, op.functions.len());
                    }
                    _ => panic!("unexpected operation type: {:#?}", operations[0]),
                }
            }
            err => panic!("expected successful txn error, got: {err:#?}"),
        }
    }
}
