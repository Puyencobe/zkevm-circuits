//! Table definitions used cross-circuits

use crate::{
    copy_circuit::util::number_or_hash_to_field,
    evm_circuit::util::rlc,
    exp_circuit::param::{OFFSET_INCREMENT, ROWS_PER_STEP},
    impl_expr,
    util::{build_tx_log_address, Challenges},
    witness::{
        Block, BlockContext, Bytecode, MptUpdateRow, MptUpdates, Rw, RwMap, RwRow, Transaction,
    },
};
use bus_mapping::circuit_input_builder::{CopyDataType, CopyEvent, CopyStep, ExpEvent};
use core::iter::once;
use eth_types::{Field, ToLittleEndian, ToScalar, Word, U256};
use gadgets::{
    binary_number::{BinaryNumberChip, BinaryNumberConfig},
    util::{split_u256, split_u256_limb64},
};
use halo2_proofs::{
    circuit::{Layouter, Region, Value},
    plonk::{Advice, Column, ConstraintSystem, Error, *},
    poly::Rotation,
};
use itertools::Itertools;
use keccak256::plain::Keccak;
use std::array;
use strum_macros::{EnumCount, EnumIter};

/// Trait used to define lookup tables
pub trait LookupTable<F: Field> {
    /// Returns the list of ALL the table columns following the table order.
    fn columns(&self) -> Vec<Column<Any>>;

    /// Returns the list of ALL the table advice columns following the table
    /// order.
    fn advice_columns(&self) -> Vec<Column<Advice>> {
        self.columns()
            .iter()
            .map(|&col| col.try_into())
            .filter_map(|res| res.ok())
            .collect()
    }

    /// Returns the String annotations associated to each column of the table.
    fn annotations(&self) -> Vec<String>;

    /// Return the list of expressions used to define the lookup table.
    fn table_exprs(&self, meta: &mut VirtualCells<F>) -> Vec<Expression<F>> {
        self.columns()
            .iter()
            .map(|&column| meta.query_any(column, Rotation::cur()))
            .collect()
    }

    /// Annotates a lookup table by passing annotations for each of it's
    /// columns.
    fn annotate_columns(&self, cs: &mut ConstraintSystem<F>) {
        self.columns()
            .iter()
            .zip(self.annotations().iter())
            .for_each(|(&col, ann)| cs.annotate_lookup_any_column(col, || ann))
    }

    /// Annotates columns of a table embedded within a circuit region.
    fn annotate_columns_in_region(&self, region: &mut Region<F>) {
        self.columns()
            .iter()
            .zip(self.annotations().iter())
            .for_each(|(&col, ann)| region.name_column(|| ann, col))
    }
}

impl<F: Field, C: Into<Column<Any>> + Copy, const W: usize> LookupTable<F> for [C; W] {
    fn table_exprs(&self, meta: &mut VirtualCells<F>) -> Vec<Expression<F>> {
        self.iter()
            .map(|column| meta.query_any(*column, Rotation::cur()))
            .collect()
    }

    fn columns(&self) -> Vec<Column<Any>> {
        self.iter().map(|&col| col.into()).collect()
    }

    fn annotations(&self) -> Vec<String> {
        vec![]
    }
}

/// Tag used to identify each field in the transaction in a row of the
/// transaction table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxFieldTag {
    /// Unused tag
    Null = 0,
    /// Nonce
    Nonce,
    /// Gas
    Gas,
    /// GasPrice
    GasPrice,
    /// CallerAddress
    CallerAddress,
    /// CalleeAddress
    CalleeAddress,
    /// IsCreate
    IsCreate,
    /// Value
    Value,
    /// CallDataLength
    CallDataLength,
    /// Gas cost for transaction call data (4 for byte == 0, 16 otherwise)
    CallDataGasCost,
    /// TxSignHash: Hash of the transaction without the signature, used for
    /// signing.
    TxSignHash,
    /// CallData
    CallData,
}
impl_expr!(TxFieldTag);

/// Alias for TxFieldTag used by EVM Circuit
pub type TxContextFieldTag = TxFieldTag;

/// Table that contains the fields of all Transactions in a block
#[derive(Clone, Debug)]
pub struct TxTable {
    /// Tx ID
    pub tx_id: Column<Advice>,
    /// Tag (TxContextFieldTag)
    pub tag: Column<Fixed>,
    /// Index for Tag = CallData
    pub index: Column<Advice>,
    /// Value
    pub value: Column<Advice>,
}

impl TxTable {
    /// Construct a new TxTable
    pub fn construct<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
        Self {
            tx_id: meta.advice_column(),
            tag: meta.fixed_column(),
            index: meta.advice_column(),
            value: meta.advice_column_in(SecondPhase),
        }
    }

    /// Assign the `TxTable` from a list of block `Transaction`s, followig the
    /// same layout that the Tx Circuit uses.
    pub fn load<F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        txs: &[Transaction],
        max_txs: usize,
        max_calldata: usize,
        challenges: &Challenges<Value<F>>,
    ) -> Result<(), Error> {
        assert!(
            txs.len() <= max_txs,
            "txs.len() <= max_txs: txs.len()={}, max_txs={}",
            txs.len(),
            max_txs
        );
        let sum_txs_calldata = txs.iter().map(|tx| tx.call_data.len()).sum();
        assert!(
            sum_txs_calldata <= max_calldata,
            "sum_txs_calldata <= max_calldata: sum_txs_calldata={}, max_calldata={}",
            sum_txs_calldata,
            max_calldata,
        );

        fn assign_row<F: Field>(
            region: &mut Region<'_, F>,
            offset: usize,
            advice_columns: &[Column<Advice>],
            tag: &Column<Fixed>,
            row: &[Value<F>; 4],
            msg: &str,
        ) -> Result<(), Error> {
            for (index, column) in advice_columns.iter().enumerate() {
                region.assign_advice(
                    || format!("tx table {} row {}", msg, offset),
                    *column,
                    offset,
                    || row[if index > 0 { index + 1 } else { index }],
                )?;
            }
            region.assign_fixed(
                || format!("tx table {} row {}", msg, offset),
                *tag,
                offset,
                || row[1],
            )?;
            Ok(())
        }

        layouter.assign_region(
            || "tx table",
            |mut region| {
                let mut offset = 0;
                let advice_columns = [self.tx_id, self.index, self.value];
                assign_row(
                    &mut region,
                    offset,
                    &advice_columns,
                    &self.tag,
                    &[(); 4].map(|_| Value::known(F::ZERO)),
                    "all-zero",
                )?;
                offset += 1;

                // Tx Table contains an initial region that has a size parametrized by max_txs
                // with all the tx data except for calldata, and then a second
                // region that has a size parametrized by max_calldata with all
                // the tx calldata.  This is required to achieve a constant fixed column tag
                // regardless of the number of input txs or the calldata size of each tx.
                let mut calldata_assignments: Vec<[Value<F>; 4]> = Vec::new();
                // Assign Tx data (all tx fields except for calldata)
                let padding_txs: Vec<_> = (txs.len()..max_txs)
                    .map(|i| Transaction {
                        id: i + 1,
                        ..Default::default()
                    })
                    .collect();
                for tx in txs.iter().chain(padding_txs.iter()) {
                    let [tx_data, tx_calldata] = tx.table_assignments(*challenges);
                    for row in tx_data {
                        assign_row(&mut region, offset, &advice_columns, &self.tag, &row, "")?;
                        offset += 1;
                    }
                    calldata_assignments.extend(tx_calldata.iter());
                }
                // Assign Tx calldata
                let padding_calldata = (sum_txs_calldata..max_calldata).map(|_| {
                    [
                        Value::known(F::ZERO),
                        Value::known(F::from(TxContextFieldTag::CallData as u64)),
                        Value::known(F::ZERO),
                        Value::known(F::ZERO),
                    ]
                });
                for row in calldata_assignments.into_iter().chain(padding_calldata) {
                    assign_row(&mut region, offset, &advice_columns, &self.tag, &row, "")?;
                    offset += 1;
                }
                Ok(())
            },
        )
    }
}

impl<F: Field> LookupTable<F> for TxTable {
    fn columns(&self) -> Vec<Column<Any>> {
        vec![
            self.tx_id.into(),
            self.tag.into(),
            self.index.into(),
            self.value.into(),
        ]
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("tx_id"),
            String::from("tag"),
            String::from("index"),
            String::from("value"),
        ]
    }

    fn table_exprs(&self, meta: &mut VirtualCells<F>) -> Vec<Expression<F>> {
        vec![
            meta.query_advice(self.tx_id, Rotation::cur()),
            meta.query_fixed(self.tag, Rotation::cur()),
            meta.query_advice(self.index, Rotation::cur()),
            meta.query_advice(self.value, Rotation::cur()),
        ]
    }
}

/// Tag to identify the operation type in a RwTable row
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter)]
pub enum RwTableTag {
    /// Start (used for padding)
    Start = 1,
    /// Stack operation
    Stack,
    /// Memory operation
    Memory,
    /// Account Storage operation
    AccountStorage,
    /// Tx Access List Account operation
    TxAccessListAccount,
    /// Tx Access List Account Storage operation
    TxAccessListAccountStorage,
    /// Tx Refund operation
    TxRefund,
    /// Account operation
    Account,
    /// Call Context operation
    CallContext,
    /// Tx Log operation
    TxLog,
    /// Tx Receipt operation
    TxReceipt,
}
impl_expr!(RwTableTag);

impl RwTableTag {
    /// Returns true if the RwTable operation is reversible
    pub fn is_reversible(self) -> bool {
        matches!(
            self,
            RwTableTag::TxAccessListAccount
                | RwTableTag::TxAccessListAccountStorage
                | RwTableTag::TxRefund
                | RwTableTag::Account
                | RwTableTag::AccountStorage
        )
    }
}

impl From<RwTableTag> for usize {
    fn from(t: RwTableTag) -> Self {
        t as usize
    }
}

/// Tag for an AccountField in RwTable
#[derive(Clone, Copy, Debug, EnumIter, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccountFieldTag {
    /// Nonce field
    Nonce = 1,
    /// Balance field
    Balance,
    /// CodeHash field
    CodeHash,
    /// NonExisting field
    NonExisting,
}
impl_expr!(AccountFieldTag);

/// Tag for a TxLogField in RwTable
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub enum TxLogFieldTag {
    /// Address field
    Address = 1,
    /// Topic field
    Topic,
    /// Data field
    Data,
}
impl_expr!(TxLogFieldTag);

/// Tag for a TxReceiptField in RwTable
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, EnumCount)]
pub enum TxReceiptFieldTag {
    /// Tx result
    PostStateOrStatus = 1,
    /// CumulativeGasUsed in the tx
    CumulativeGasUsed,
    /// Number of logs in the tx
    LogLength,
}
impl_expr!(TxReceiptFieldTag);

/// Tag for a CallContextField in RwTable
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub enum CallContextFieldTag {
    /// RwCounterEndOfReversion
    RwCounterEndOfReversion = 1,
    /// CallerId
    CallerId,
    /// TxId
    TxId,
    /// Depth
    Depth,
    /// CallerAddress
    CallerAddress,
    /// CalleeAddress
    CalleeAddress,
    /// CallDataOffset
    CallDataOffset,
    /// CallDataLength
    CallDataLength,
    /// ReturnDataOffset
    ReturnDataOffset,
    /// ReturnDataLength
    ReturnDataLength,
    /// Value
    Value,
    /// IsSuccess
    IsSuccess,
    /// IsPersistent
    IsPersistent,
    /// IsStatic
    IsStatic,

    /// LastCalleeId
    LastCalleeId,
    /// LastCalleeReturnDataOffset
    LastCalleeReturnDataOffset,
    /// LastCalleeReturnDataLength
    LastCalleeReturnDataLength,

    /// IsRoot
    IsRoot,
    /// IsCreate
    IsCreate,
    /// CodeHash
    CodeHash,
    /// ProgramCounter
    ProgramCounter,
    /// StackPointer
    StackPointer,
    /// GasLeft
    GasLeft,
    /// MemorySize
    MemorySize,
    /// ReversibleWriteCounter
    ReversibleWriteCounter,
}
impl_expr!(CallContextFieldTag);

/// The RwTable shared between EVM Circuit and State Circuit, which contains
/// traces of the EVM state operations.
#[derive(Clone, Copy, Debug)]
pub struct RwTable {
    /// Read Write Counter
    pub rw_counter: Column<Advice>,
    /// Is Write
    pub is_write: Column<Advice>,
    /// Tag
    pub tag: Column<Advice>,
    /// Key1 (Id)
    pub id: Column<Advice>,
    /// Key2 (Address)
    pub address: Column<Advice>,
    /// Key3 (FieldTag)
    pub field_tag: Column<Advice>,
    /// Key3 (StorageKey)
    pub storage_key: Column<Advice>,
    /// Value
    pub value: Column<Advice>,
    /// Value Previous
    pub value_prev: Column<Advice>,
    /// Aux1
    pub aux1: Column<Advice>,
    /// Aux2 (Committed Value)
    pub aux2: Column<Advice>,
}

impl<F: Field> LookupTable<F> for RwTable {
    fn columns(&self) -> Vec<Column<Any>> {
        vec![
            self.rw_counter.into(),
            self.is_write.into(),
            self.tag.into(),
            self.id.into(),
            self.address.into(),
            self.field_tag.into(),
            self.storage_key.into(),
            self.value.into(),
            self.value_prev.into(),
            self.aux1.into(),
            self.aux2.into(),
        ]
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("rw_counter"),
            String::from("is_write"),
            String::from("tag"),
            String::from("id"),
            String::from("address"),
            String::from("field_tag"),
            String::from("storage_key"),
            String::from("value"),
            String::from("value_prev"),
            String::from("aux1"),
            String::from("aux2"),
        ]
    }
}
impl RwTable {
    /// Construct a new RwTable
    pub fn construct<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
        Self {
            rw_counter: meta.advice_column(),
            is_write: meta.advice_column(),
            tag: meta.advice_column(),
            id: meta.advice_column(),
            address: meta.advice_column(),
            field_tag: meta.advice_column(),
            storage_key: meta.advice_column_in(SecondPhase),
            value: meta.advice_column_in(SecondPhase),
            value_prev: meta.advice_column_in(SecondPhase),
            // It seems that aux1 for the moment is not using randomness
            // TODO check in a future review
            aux1: meta.advice_column_in(SecondPhase),
            aux2: meta.advice_column_in(SecondPhase),
        }
    }
    fn assign<F: Field>(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        row: &RwRow<Value<F>>,
    ) -> Result<(), Error> {
        for (column, value) in [
            (self.rw_counter, row.rw_counter),
            (self.is_write, row.is_write),
            (self.tag, row.tag),
            (self.id, row.id),
            (self.address, row.address),
            (self.field_tag, row.field_tag),
            (self.storage_key, row.storage_key),
            (self.value, row.value),
            (self.value_prev, row.value_prev),
            (self.aux1, row.aux1),
            (self.aux2, row.aux2),
        ] {
            region.assign_advice(|| "assign rw row on rw table", column, offset, || value)?;
        }
        Ok(())
    }

    /// Assign the `RwTable` from a `RwMap`, following the same
    /// table layout that the State Circuit uses.
    pub fn load<F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        rws: &[Rw],
        n_rows: usize,
        challenges: Value<F>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "rw table",
            |mut region| self.load_with_region(&mut region, rws, n_rows, challenges),
        )
    }

    pub(crate) fn load_with_region<F: Field>(
        &self,
        region: &mut Region<'_, F>,
        rws: &[Rw],
        n_rows: usize,
        challenges: Value<F>,
    ) -> Result<(), Error> {
        let (rows, _) = RwMap::table_assignments_prepad(rws, n_rows);
        for (offset, row) in rows.iter().enumerate() {
            self.assign(region, offset, &row.table_assignment(challenges))?;
        }
        Ok(())
    }
}

/// The types of proofs in the MPT table
#[derive(Clone, Copy, Debug)]
pub enum MPTProofType {
    /// Nonce updated
    NonceMod = AccountFieldTag::Nonce as isize,
    /// Balance updated
    BalanceMod = AccountFieldTag::Balance as isize,
    /// Code hash exists
    CodeHashMod = AccountFieldTag::CodeHash as isize,
    /// Account does not exist
    NonExistingAccountProof = AccountFieldTag::NonExisting as isize,
    /// Storage updated
    StorageMod,
    /// Storage does not exist
    NonExistingStorageProof,
}
impl_expr!(MPTProofType);

impl From<AccountFieldTag> for MPTProofType {
    fn from(tag: AccountFieldTag) -> Self {
        match tag {
            AccountFieldTag::Nonce => Self::NonceMod,
            AccountFieldTag::Balance => Self::BalanceMod,
            AccountFieldTag::CodeHash => Self::CodeHashMod,
            AccountFieldTag::NonExisting => Self::NonExistingAccountProof,
        }
    }
}

/// The MptTable shared between MPT Circuit and State Circuit
#[derive(Clone, Copy, Debug)]
pub struct MptTable([Column<Advice>; 7]);

impl<F: Field> LookupTable<F> for MptTable {
    fn columns(&self) -> Vec<Column<Any>> {
        self.0.iter().map(|&col| col.into()).collect()
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("address"),
            String::from("storage_key"),
            String::from("proof_type"),
            String::from("new_root"),
            String::from("old_root"),
            String::from("new_value"),
            String::from("old_value"),
        ]
    }
}

impl MptTable {
    /// Construct a new MptTable
    pub(crate) fn construct<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
        Self([
            meta.advice_column(),               // Address
            meta.advice_column_in(SecondPhase), // Storage key
            meta.advice_column(),               // Proof type
            meta.advice_column_in(SecondPhase), // New root
            meta.advice_column_in(SecondPhase), // Old root
            meta.advice_column_in(SecondPhase), // New value
            meta.advice_column_in(SecondPhase), // Old value
        ])
    }

    pub(crate) fn assign<F: Field>(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        row: &MptUpdateRow<Value<F>>,
    ) -> Result<(), Error> {
        for (column, value) in self.0.iter().zip_eq(row.values()) {
            region.assign_advice(|| "assign mpt table row value", *column, offset, || *value)?;
        }
        Ok(())
    }

    pub(crate) fn load<F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        updates: &MptUpdates,
        randomness: Value<F>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "mpt table",
            |mut region| self.load_with_region(&mut region, updates, randomness),
        )
    }

    pub(crate) fn load_with_region<F: Field>(
        &self,
        region: &mut Region<'_, F>,
        updates: &MptUpdates,
        randomness: Value<F>,
    ) -> Result<(), Error> {
        for (offset, row) in updates.table_assignments(randomness).iter().enumerate() {
            self.assign(region, offset, row)?;
        }
        Ok(())
    }
}

/// Tag to identify the field in a Bytecode Table row
#[derive(Clone, Copy, Debug)]
pub enum BytecodeFieldTag {
    /// Header field
    Header,
    /// Byte field
    Byte,
}
impl_expr!(BytecodeFieldTag);

/// Table with Bytecode indexed by its Code Hash
#[derive(Clone, Debug)]
pub struct BytecodeTable {
    /// Code Hash
    pub code_hash: Column<Advice>,
    /// Tag
    pub tag: Column<Advice>,
    /// Index
    pub index: Column<Advice>,
    /// Is Code is true when the byte is not an argument to a PUSH* instruction.
    pub is_code: Column<Advice>,
    /// Value
    pub value: Column<Advice>,
}

impl BytecodeTable {
    /// Construct a new BytecodeTable
    pub fn construct<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
        let [tag, index, is_code, value] = array::from_fn(|_| meta.advice_column());
        let code_hash = meta.advice_column_in(SecondPhase);
        Self {
            code_hash,
            tag,
            index,
            is_code,
            value,
        }
    }

    /// Assign the `BytecodeTable` from a list of bytecodes, followig the same
    /// table layout that the Bytecode Circuit uses.
    pub fn load<'a, F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        bytecodes: impl IntoIterator<Item = &'a Bytecode> + Clone,
        challenges: &Challenges<Value<F>>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "bytecode table",
            |mut region| {
                let mut offset = 0;
                for column in <BytecodeTable as LookupTable<F>>::advice_columns(self) {
                    region.assign_advice(
                        || "bytecode table all-zero row",
                        column,
                        offset,
                        || Value::known(F::ZERO),
                    )?;
                }
                offset += 1;

                let bytecode_table_columns =
                    <BytecodeTable as LookupTable<F>>::advice_columns(self);
                for bytecode in bytecodes.clone() {
                    for row in bytecode.table_assignments(challenges) {
                        for (&column, value) in bytecode_table_columns.iter().zip_eq(row) {
                            region.assign_advice(
                                || format!("bytecode table row {}", offset),
                                column,
                                offset,
                                || value,
                            )?;
                        }
                        offset += 1;
                    }
                }
                Ok(())
            },
        )
    }
}

impl<F: Field> LookupTable<F> for BytecodeTable {
    fn columns(&self) -> Vec<Column<Any>> {
        vec![
            self.code_hash.into(),
            self.tag.into(),
            self.index.into(),
            self.is_code.into(),
            self.value.into(),
        ]
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("code_hash"),
            String::from("tag"),
            String::from("index"),
            String::from("is_code"),
            String::from("value"),
        ]
    }
}

/// Tag to identify the field in a Block Table row
// Keep the sequence consistent with OpcodeId for scalar
#[derive(Clone, Copy, Debug)]
pub enum BlockContextFieldTag {
    /// Coinbase field
    Coinbase = 1,
    /// Timestamp field
    Timestamp,
    /// Number field
    Number,
    /// Difficulty field
    Difficulty,
    /// Gas Limit field
    GasLimit,
    /// Base Fee field
    BaseFee = 8,
    /// Block Hash field
    BlockHash,
    /// Chain ID field.  Although this is not a field in the block header, we
    /// add it here for convenience.
    ChainId,
}
impl_expr!(BlockContextFieldTag);

/// Table with Block header fields
#[derive(Clone, Debug)]
pub struct BlockTable {
    /// Tag
    pub tag: Column<Advice>,
    /// Index
    pub index: Column<Advice>,
    /// Value
    pub value: Column<Advice>,
}

impl BlockTable {
    /// Construct a new BlockTable
    pub fn construct<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
        Self {
            tag: meta.advice_column(),
            index: meta.advice_column(),
            value: meta.advice_column_in(SecondPhase),
        }
    }

    /// Assign the `BlockTable` from a `BlockContext`.
    pub fn load<F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &BlockContext,
        randomness: Value<F>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "block table",
            |mut region| {
                let mut offset = 0;
                for column in <BlockTable as LookupTable<F>>::advice_columns(self) {
                    region.assign_advice(
                        || "block table all-zero row",
                        column,
                        offset,
                        || Value::known(F::ZERO),
                    )?;
                }
                offset += 1;

                let block_table_columns = <BlockTable as LookupTable<F>>::advice_columns(self);
                for row in block.table_assignments(randomness) {
                    for (&column, value) in block_table_columns.iter().zip_eq(row) {
                        region.assign_advice(
                            || format!("block table row {}", offset),
                            column,
                            offset,
                            || value,
                        )?;
                    }
                    offset += 1;
                }

                Ok(())
            },
        )
    }
}

impl<F: Field> LookupTable<F> for BlockTable {
    fn columns(&self) -> Vec<Column<Any>> {
        vec![self.tag.into(), self.index.into(), self.value.into()]
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("tag"),
            String::from("index"),
            String::from("value"),
        ]
    }
}

/// Keccak Table, used to verify keccak hashing from RLC'ed input.
#[derive(Clone, Debug)]
pub struct KeccakTable {
    /// True when the row is enabled
    pub is_enabled: Column<Advice>,
    /// Byte array input as `RLC(reversed(input))`
    pub input_rlc: Column<Advice>, // RLC of input bytes
    /// Byte array input length
    pub input_len: Column<Advice>,
    /// RLC of the hash result
    pub output_rlc: Column<Advice>, // RLC of hash of input bytes
}

impl<F: Field> LookupTable<F> for KeccakTable {
    fn columns(&self) -> Vec<Column<Any>> {
        vec![
            self.is_enabled.into(),
            self.input_rlc.into(),
            self.input_len.into(),
            self.output_rlc.into(),
        ]
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("is_enabled"),
            String::from("input_rlc"),
            String::from("input_len"),
            String::from("output_rlc"),
        ]
    }
}

impl KeccakTable {
    /// Construct a new KeccakTable
    pub fn construct<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
        Self {
            is_enabled: meta.advice_column(),
            input_rlc: meta.advice_column_in(SecondPhase),
            input_len: meta.advice_column(),
            output_rlc: meta.advice_column_in(SecondPhase),
        }
    }

    /// Generate the keccak table assignments from a byte array input.
    pub fn assignments<F: Field>(
        input: &[u8],
        challenges: &Challenges<Value<F>>,
    ) -> Vec<[Value<F>; 4]> {
        let input_rlc = challenges
            .keccak_input()
            .map(|challenge| rlc::value(input.iter().rev(), challenge));
        let input_len = F::from(input.len() as u64);
        let mut keccak = Keccak::default();
        keccak.update(input);
        let output = keccak.digest();
        let output_rlc = challenges.evm_word().map(|challenge| {
            rlc::value(
                &Word::from_big_endian(output.as_slice()).to_le_bytes(),
                challenge,
            )
        });

        vec![[
            Value::known(F::ONE),
            input_rlc,
            Value::known(input_len),
            output_rlc,
        ]]
    }

    /// Assign a table row for keccak table
    pub fn assign_row<F: Field>(
        &self,
        region: &mut Region<F>,
        offset: usize,
        values: [Value<F>; 4],
    ) -> Result<(), Error> {
        for (&column, value) in <KeccakTable as LookupTable<F>>::advice_columns(self)
            .iter()
            .zip(values.iter())
        {
            region.assign_advice(|| format!("assign {}", offset), column, offset, || *value)?;
        }
        Ok(())
    }

    /// Provide this function for the case that we want to consume a keccak
    /// table but without running the full keccak circuit
    pub fn dev_load<'a, F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        inputs: impl IntoIterator<Item = &'a Vec<u8>> + Clone,
        challenges: &Challenges<Value<F>>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "keccak table",
            |mut region| {
                let mut offset = 0;
                for column in <KeccakTable as LookupTable<F>>::advice_columns(self) {
                    region.assign_advice(
                        || "keccak table all-zero row",
                        column,
                        offset,
                        || Value::known(F::ZERO),
                    )?;
                }
                offset += 1;

                let keccak_table_columns = <KeccakTable as LookupTable<F>>::advice_columns(self);
                for input in inputs.clone() {
                    for row in Self::assignments(input, challenges) {
                        // let mut column_index = 0;
                        for (&column, value) in keccak_table_columns.iter().zip_eq(row) {
                            region.assign_advice(
                                || format!("keccak table row {}", offset),
                                column,
                                offset,
                                || value,
                            )?;
                        }
                        offset += 1;
                    }
                }
                Ok(())
            },
        )
    }

    /// returns matchings between the circuit columns passed as parameters and
    /// the table collumns
    pub fn match_columns(
        &self,
        value_rlc: Column<Advice>,
        length: Column<Advice>,
        code_hash: Column<Advice>,
    ) -> Vec<(Column<Advice>, Column<Advice>)> {
        vec![
            (value_rlc, self.input_rlc),
            (length, self.input_len),
            (code_hash, self.output_rlc),
        ]
    }
}

/// Copy Table, used to verify copies of byte chunks between Memory, Bytecode,
/// TxLogs and TxCallData.
#[derive(Clone, Copy, Debug)]
pub struct CopyTable {
    /// Whether the row is the first read-write pair for a copy event.
    pub is_first: Column<Advice>,
    /// The relevant ID for the read-write row, represented as a random linear
    /// combination. The ID may be one of the below:
    /// 1. Call ID/Caller ID for CopyDataType::Memory
    /// 2. RLC encoding of bytecode hash for CopyDataType::Bytecode
    /// 3. Transaction ID for CopyDataType::TxCalldata, CopyDataType::TxLog
    pub id: Column<Advice>,
    /// The source/destination address for this copy step.  Can be memory
    /// address, byte index in the bytecode, tx call data, and tx log data.
    pub addr: Column<Advice>,
    /// The end of the source buffer for the copy event.  Any data read from an
    /// address greater than or equal to this value will be 0.
    pub src_addr_end: Column<Advice>,
    /// The number of bytes left to be copied.
    pub bytes_left: Column<Advice>,
    /// An accumulator value in the RLC representation. This is used for
    /// specific purposes, for instance, when `tag == CopyDataType::RlcAcc`.
    /// Having an additional column for the `rlc_acc` simplifies the lookup
    /// to copy table.
    pub rlc_acc: Column<Advice>,
    /// The associated read-write counter for this row.
    pub rw_counter: Column<Advice>,
    /// Decrementing counter denoting reverse read-write counter.
    pub rwc_inc_left: Column<Advice>,
    /// Binary chip to constrain the copy table conditionally depending on the
    /// current row's tag, whether it is Bytecode, Memory, TxCalldata or
    /// TxLog.
    pub tag: BinaryNumberConfig<CopyDataType, 3>,
}

type CopyTableRow<F> = [(Value<F>, &'static str); 8];
type CopyCircuitRow<F> = [(Value<F>, &'static str); 4];

impl CopyTable {
    /// Construct a new CopyTable
    pub fn construct<F: Field>(meta: &mut ConstraintSystem<F>, q_enable: Column<Fixed>) -> Self {
        Self {
            is_first: meta.advice_column(),
            id: meta.advice_column_in(SecondPhase),
            tag: BinaryNumberChip::configure(meta, q_enable, None),
            addr: meta.advice_column(),
            src_addr_end: meta.advice_column(),
            bytes_left: meta.advice_column(),
            rlc_acc: meta.advice_column_in(SecondPhase),
            rw_counter: meta.advice_column(),
            rwc_inc_left: meta.advice_column(),
        }
    }

    /// Generate the copy table and copy circuit assignments from a copy event.
    pub fn assignments<F: Field>(
        copy_event: &CopyEvent,
        challenges: Challenges<Value<F>>,
    ) -> Vec<(CopyDataType, CopyTableRow<F>, CopyCircuitRow<F>)> {
        let mut assignments = Vec::new();
        // rlc_acc
        let rlc_acc = if copy_event.dst_type == CopyDataType::RlcAcc {
            let values = copy_event
                .bytes
                .iter()
                .map(|(value, _)| *value)
                .collect::<Vec<u8>>();
            challenges
                .keccak_input()
                .map(|keccak_input| rlc::value(values.iter().rev(), keccak_input))
        } else {
            Value::known(F::ZERO)
        };
        let mut value_acc = Value::known(F::ZERO);
        for (step_idx, (is_read_step, copy_step)) in copy_event
            .bytes
            .iter()
            .flat_map(|(value, is_code)| {
                let read_step = CopyStep {
                    value: *value,
                    is_code: if copy_event.src_type == CopyDataType::Bytecode {
                        Some(*is_code)
                    } else {
                        None
                    },
                };
                let write_step = CopyStep {
                    value: *value,
                    is_code: if copy_event.dst_type == CopyDataType::Bytecode {
                        Some(*is_code)
                    } else {
                        None
                    },
                };
                once((true, read_step)).chain(once((false, write_step)))
            })
            .enumerate()
        {
            // is_first
            let is_first = Value::known(if step_idx == 0 { F::ONE } else { F::ZERO });
            // is last
            let is_last = if step_idx == copy_event.bytes.len() * 2 - 1 {
                Value::known(F::ONE)
            } else {
                Value::known(F::ZERO)
            };

            // id
            let id = if is_read_step {
                number_or_hash_to_field(&copy_event.src_id, challenges.evm_word())
            } else {
                number_or_hash_to_field(&copy_event.dst_id, challenges.evm_word())
            };

            // tag binary bumber chip
            let tag = if is_read_step {
                copy_event.src_type
            } else {
                copy_event.dst_type
            };

            // addr
            let copy_step_addr: u64 =
                if is_read_step {
                    copy_event.src_addr
                } else {
                    copy_event.dst_addr
                } + (u64::try_from(step_idx).unwrap() - if is_read_step { 0 } else { 1 }) / 2u64;

            let addr = if tag == CopyDataType::TxLog {
                Value::known(
                    build_tx_log_address(
                        copy_step_addr,
                        TxLogFieldTag::Data,
                        copy_event.log_id.unwrap(),
                    )
                    .to_scalar()
                    .unwrap(),
                )
            } else {
                Value::known(F::from(copy_step_addr))
            };

            // bytes_left
            let bytes_left = u64::try_from(copy_event.bytes.len() * 2 - step_idx).unwrap() / 2;
            // value
            let value = if copy_event.dst_type == CopyDataType::RlcAcc {
                if is_read_step {
                    Value::known(F::from(copy_step.value as u64))
                } else {
                    value_acc = value_acc * challenges.keccak_input()
                        + Value::known(F::from(copy_step.value as u64));
                    value_acc
                }
            } else {
                Value::known(F::from(copy_step.value as u64))
            };
            // is_pad
            let is_pad = Value::known(F::from(
                (is_read_step && copy_step_addr >= copy_event.src_addr_end) as u64,
            ));

            // is_code
            let is_code = Value::known(copy_step.is_code.map_or(F::ZERO, |v| F::from(v as u64)));

            assignments.push((
                tag,
                [
                    (is_first, "is_first"),
                    (id, "id"),
                    (addr, "addr"),
                    (
                        Value::known(F::from(copy_event.src_addr_end)),
                        "src_addr_end",
                    ),
                    (Value::known(F::from(bytes_left)), "bytes_left"),
                    (rlc_acc, "rlc_acc"),
                    (
                        Value::known(F::from(copy_event.rw_counter(step_idx))),
                        "rw_counter",
                    ),
                    (
                        Value::known(F::from(copy_event.rw_counter_increase_left(step_idx))),
                        "rwc_inc_left",
                    ),
                ],
                [
                    (is_last, "is_last"),
                    (value, "value"),
                    (is_pad, "is_pad"),
                    (is_code, "is_code"),
                ],
            ));
        }
        assignments
    }

    /// Assign the `CopyTable` from a `Block`.
    pub fn load<F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &Block<F>,
        challenges: &Challenges<Value<F>>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "copy table",
            |mut region| {
                let mut offset = 0;
                for column in <CopyTable as LookupTable<F>>::advice_columns(self) {
                    region.assign_advice(
                        || "copy table all-zero row",
                        column,
                        offset,
                        || Value::known(F::ZERO),
                    )?;
                }
                offset += 1;

                let tag_chip = BinaryNumberChip::construct(self.tag);
                let copy_table_columns = <CopyTable as LookupTable<F>>::advice_columns(self);
                for copy_event in block.copy_events.iter() {
                    for (tag, row, _) in Self::assignments(copy_event, *challenges) {
                        for (&column, (value, label)) in copy_table_columns.iter().zip_eq(row) {
                            region.assign_advice(
                                || format!("{} at row: {}", label, offset),
                                column,
                                offset,
                                || value,
                            )?;
                        }
                        tag_chip.assign(&mut region, offset, &tag)?;
                        offset += 1;
                    }
                }

                Ok(())
            },
        )
    }
}

impl<F: Field> LookupTable<F> for CopyTable {
    fn columns(&self) -> Vec<Column<Any>> {
        vec![
            self.is_first.into(),
            self.id.into(),
            self.addr.into(),
            self.src_addr_end.into(),
            self.bytes_left.into(),
            self.rlc_acc.into(),
            self.rw_counter.into(),
            self.rwc_inc_left.into(),
        ]
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("is_first"),
            String::from("id"),
            String::from("addr"),
            String::from("src_addr_end"),
            String::from("bytes_left"),
            String::from("rlc_acc"),
            String::from("rw_counter"),
            String::from("rwc_inc_left"),
        ]
    }

    fn table_exprs(&self, meta: &mut VirtualCells<F>) -> Vec<Expression<F>> {
        vec![
            meta.query_advice(self.is_first, Rotation::cur()),
            meta.query_advice(self.id, Rotation::cur()), // src_id
            self.tag.value(Rotation::cur())(meta),       // src_tag
            meta.query_advice(self.id, Rotation::next()), // dst_id
            self.tag.value(Rotation::next())(meta),      // dst_tag
            meta.query_advice(self.addr, Rotation::cur()), // src_addr
            meta.query_advice(self.src_addr_end, Rotation::cur()), // src_addr_end
            meta.query_advice(self.addr, Rotation::next()), // dst_addr
            meta.query_advice(self.bytes_left, Rotation::cur()), // length
            meta.query_advice(self.rlc_acc, Rotation::cur()), // rlc_acc
            meta.query_advice(self.rw_counter, Rotation::cur()), // rw_counter
            meta.query_advice(self.rwc_inc_left, Rotation::cur()), // rwc_inc_left
        ]
    }
}

/// Lookup table within the Exponentiation circuit.
#[derive(Clone, Copy, Debug)]
pub struct ExpTable {
    /// Whether the row is the start of a step.
    pub is_step: Column<Fixed>,
    /// An identifier for every exponentiation trace, at the moment this is the
    /// read-write counter at the time of the lookups done to the
    /// exponentiation table.
    pub identifier: Column<Advice>,
    /// Whether this row is the last row in the exponentiation operation's
    /// trace.
    pub is_last: Column<Advice>,
    /// The integer base of the exponentiation.
    pub base_limb: Column<Advice>,
    /// The integer exponent of the exponentiation.
    pub exponent_lo_hi: Column<Advice>,
    /// The intermediate result of exponentiation by squaring.
    pub exponentiation_lo_hi: Column<Advice>,
}

impl ExpTable {
    /// Construct the Exponentiation table.
    pub fn construct<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
        Self {
            is_step: meta.fixed_column(),
            identifier: meta.advice_column(),
            is_last: meta.advice_column(),
            base_limb: meta.advice_column(),
            exponent_lo_hi: meta.advice_column(),
            exponentiation_lo_hi: meta.advice_column(),
        }
    }

    /// Given an exponentiation event and randomness, get assignments to the
    /// exponentiation table.
    pub fn assignments<F: Field>(exp_event: &ExpEvent) -> Vec<[F; 5]> {
        let mut assignments = Vec::new();
        let base_limbs = split_u256_limb64(&exp_event.base);
        let identifier = F::from(exp_event.identifier as u64);
        let mut exponent = exp_event.exponent;
        for (step_idx, exp_step) in exp_event.steps.iter().rev().enumerate() {
            let is_last = if step_idx == exp_event.steps.len() - 1 {
                F::ONE
            } else {
                F::ZERO
            };
            let (exp_lo, exp_hi) = split_u256(&exp_step.d);
            let (exponent_lo, exponent_hi) = split_u256(&exponent);

            // row 1
            assignments.push([
                identifier,
                is_last,
                base_limbs[0].as_u64().into(),
                exponent_lo
                    .to_scalar()
                    .expect("exponent should fit to scalar"),
                exp_lo
                    .to_scalar()
                    .expect("exponentiation lo should fit to scalar"),
            ]);
            // row 2
            assignments.push([
                identifier,
                F::ZERO,
                base_limbs[1].as_u64().into(),
                exponent_hi
                    .to_scalar()
                    .expect("exponent hi should fit to scalar"),
                exp_hi
                    .to_scalar()
                    .expect("exponentiation hi should fit to scalar"),
            ]);
            // row 3
            assignments.push([
                identifier,
                F::ZERO,
                base_limbs[2].as_u64().into(),
                F::ZERO,
                F::ZERO,
            ]);
            // row 4
            assignments.push([
                identifier,
                F::ZERO,
                base_limbs[3].as_u64().into(),
                F::ZERO,
                F::ZERO,
            ]);
            for _ in ROWS_PER_STEP..OFFSET_INCREMENT {
                assignments.push([F::ZERO, F::ZERO, F::ZERO, F::ZERO, F::ZERO]);
            }

            // update intermediate exponent.
            let (exponent_div2, remainder) = exponent.div_mod(U256::from(2));
            if remainder.is_zero() {
                // exponent is even
                exponent = exponent_div2;
            } else {
                // exponent is odd
                exponent = exponent - 1;
            }
        }
        assignments
    }

    /// Assign witness data from a block to the exponentiation table.
    pub fn load<F: Field>(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &Block<F>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "exponentiation table",
            |mut region| {
                let mut offset = 0;
                let exp_table_columns = <ExpTable as LookupTable<F>>::advice_columns(self);
                for exp_event in block.exp_events.iter() {
                    for row in Self::assignments::<F>(exp_event) {
                        for (&column, value) in exp_table_columns.iter().zip_eq(row) {
                            region.assign_advice(
                                || format!("exponentiation table row {}", offset),
                                column,
                                offset,
                                || Value::known(value),
                            )?;
                        }
                        let is_step = if offset % OFFSET_INCREMENT == 0 {
                            F::ONE
                        } else {
                            F::ZERO
                        };
                        region.assign_fixed(
                            || format!("exponentiation table row {}", offset),
                            self.is_step,
                            offset,
                            || Value::known(is_step),
                        )?;
                        offset += 1;
                    }
                }

                // pad an empty row
                let row = [F::from_u128(0); 5];
                for (column, value) in exp_table_columns.iter().zip_eq(row) {
                    region.assign_advice(
                        || format!("exponentiation table row {}", offset),
                        *column,
                        offset,
                        || Value::known(value),
                    )?;
                }

                Ok(())
            },
        )
    }
}

impl<F: Field> LookupTable<F> for ExpTable {
    fn columns(&self) -> Vec<Column<Any>> {
        vec![
            self.is_step.into(),
            self.identifier.into(),
            self.is_last.into(),
            self.base_limb.into(),
            self.exponent_lo_hi.into(),
            self.exponentiation_lo_hi.into(),
        ]
    }

    fn annotations(&self) -> Vec<String> {
        vec![
            String::from("is_step"),
            String::from("identifier"),
            String::from("is_last"),
            String::from("base_limb"),
            String::from("exponent_lo_hi"),
            String::from("exponentiation_lo_hi"),
        ]
    }

    fn table_exprs(&self, meta: &mut VirtualCells<F>) -> Vec<Expression<F>> {
        vec![
            meta.query_fixed(self.is_step, Rotation::cur()),
            meta.query_advice(self.identifier, Rotation::cur()),
            meta.query_advice(self.is_last, Rotation::cur()),
            meta.query_advice(self.base_limb, Rotation::cur()),
            meta.query_advice(self.base_limb, Rotation::next()),
            meta.query_advice(self.base_limb, Rotation(2)),
            meta.query_advice(self.base_limb, Rotation(3)),
            meta.query_advice(self.exponent_lo_hi, Rotation::cur()),
            meta.query_advice(self.exponent_lo_hi, Rotation::next()),
            meta.query_advice(self.exponentiation_lo_hi, Rotation::cur()),
            meta.query_advice(self.exponentiation_lo_hi, Rotation::next()),
        ]
    }
}
