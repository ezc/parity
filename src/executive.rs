//! Transaction Execution environment.
use common::*;
use state::*;
use engine::*;
use evm::{self, Schedule, Factory, Ext};

/// Returns new address created from address and given nonce.
pub fn contract_address(address: &Address, nonce: &U256) -> Address {
	let mut stream = RlpStream::new_list(2);
	stream.append(address);
	stream.append(nonce);
	From::from(stream.out().sha3())
}

/// State changes which should be applied in finalize,
/// after transaction is fully executed.
pub struct Substate {
	/// Any accounts that have suicided.
	suicides: HashSet<Address>,
	/// Any logs.
	logs: Vec<LogEntry>,
	/// Refund counter of SSTORE nonzero->zero.
	refunds_count: U256,
	/// True if transaction, or one of its subcalls runs out of gas.
	out_of_gas: bool,
	/// Created contracts.
	contracts_created: Vec<Address>
}

impl Substate {
	/// Creates new substate.
	pub fn new() -> Self {
		Substate {
			suicides: HashSet::new(),
			logs: vec![],
			refunds_count: U256::zero(),
			out_of_gas: false,
			contracts_created: vec![]
		}
	}

	pub fn out_of_gas(&self) -> bool { self.out_of_gas }
}

/// Transaction execution receipt.
#[derive(Debug)]
pub struct Executed {
	/// Gas paid up front for execution of transaction.
	pub gas: U256,
	/// Gas used during execution of transaction.
	pub gas_used: U256,
	/// Gas refunded after the execution of transaction. 
	/// To get gas that was required up front, add `refunded` and `gas_used`.
	pub refunded: U256,
	/// Cumulative gas used in current block so far.
	/// 
	/// `cumulative_gas_used = gas_used(t0) + gas_used(t1) + ... gas_used(tn)`
	///
	/// where `tn` is current transaction.
	pub cumulative_gas_used: U256,
	/// Vector of logs generated by transaction.
	pub logs: Vec<LogEntry>,
	/// Execution ended running out of gas.
	pub out_of_gas: bool,
	/// Addresses of contracts created during execution of transaction.
	/// Ordered from earliest creation.
	/// 
	/// eg. sender creates contract A and A in constructor creates contract B 
	/// 
	/// B creation ends first, and it will be the first element of the vector.
	pub contracts_created: Vec<Address>
}

/// Transaction execution result.
pub type ExecutionResult = Result<Executed, ExecutionError>;

/// Transaction executor.
pub struct Executive<'a> {
	state: &'a mut State,
	info: &'a EnvInfo,
	engine: &'a Engine,
	depth: usize
}

impl<'a> Executive<'a> {
	/// Basic constructor.
	pub fn new(state: &'a mut State, info: &'a EnvInfo, engine: &'a Engine) -> Self {
		Executive::new_with_depth(state, info, engine, 0)
	}

	/// Populates executive from parent properties. Increments executive depth.
	fn from_parent(state: &'a mut State, info: &'a EnvInfo, engine: &'a Engine, depth: usize) -> Self {
		Executive::new_with_depth(state, info, engine, depth + 1)
	}

	/// Helper constructor. Should be used to create `Executive` with desired depth.
	/// Private.
	fn new_with_depth(state: &'a mut State, info: &'a EnvInfo, engine: &'a Engine, depth: usize) -> Self {
		Executive {
			state: state,
			info: info,
			engine: engine,
			depth: depth
		}
	}

	/// This funtion should be used to execute transaction.
	pub fn transact(&mut self, t: &Transaction) -> Result<Executed, Error> {
		let sender = try!(t.sender());
		let nonce = self.state.nonce(&sender);

		// TODO: error on base gas required

		// validate transaction nonce
		if t.nonce != nonce {
			return Err(From::from(ExecutionError::InvalidNonce { expected: nonce, is: t.nonce }));
		}
		
		// validate if transaction fits into given block
		if self.info.gas_used + t.gas > self.info.gas_limit {
			return Err(From::from(ExecutionError::BlockGasLimitReached { 
				gas_limit: self.info.gas_limit, 
				gas_used: self.info.gas_used, 
				gas: t.gas 
			}));
		}

		// TODO: we might need bigints here, or at least check overflows.
		let balance = self.state.balance(&sender);
		let gas_cost = U512::from(t.gas) * U512::from(t.gas_price);
		let total_cost = U512::from(t.value) + gas_cost;

		// avoid unaffordable transactions
		if U512::from(balance) < total_cost {
			return Err(From::from(ExecutionError::NotEnoughCash { required: total_cost, is: U512::from(balance) }));
		}

		// NOTE: there can be no invalid transactions from this point.
		self.state.inc_nonce(&sender);
		self.state.sub_balance(&sender, &U256::from(gas_cost));

		let mut substate = Substate::new();

		let schedule = self.engine.schedule(self.info);
		let init_gas = t.gas - U256::from(t.gas_required(&schedule));

		let res = match t.action() {
			&Action::Create => {
				let params = ActionParams {
					address: contract_address(&sender, &nonce),
					sender: sender.clone(),
					origin: sender.clone(),
					gas: init_gas,
					gas_price: t.gas_price,
					value: t.value,
					code: t.data.clone(),
					data: vec![],
				};
				self.create(&params, &mut substate)
			},
			&Action::Call(ref address) => {
				let params = ActionParams {
					address: address.clone(),
					sender: sender.clone(),
					origin: sender.clone(),
					gas: init_gas,
					gas_price: t.gas_price,
					value: t.value,
					code: self.state.code(address).unwrap_or(vec![]),
					data: t.data.clone(),
				};
				// TODO: move output upstream
				let mut out = vec![];
				self.call(&params, &mut substate, BytesRef::Flexible(&mut out))
			}
		};

		// finalize here!
		Ok(try!(self.finalize(t, substate, res)))
	}

	/// Calls contract function with given contract params.
	/// NOTE. It does not finalize the transaction (doesn't do refunds, nor suicides).
	/// Modifies the substate and the output.
	/// Returns either gas_left or `evm::Error`.
	pub fn call(&mut self, params: &ActionParams, substate: &mut Substate, mut output: BytesRef) -> evm::Result {
		// backup used in case of running out of gas
		let backup = self.state.clone();

		// at first, transfer value to destination
		self.state.transfer_balance(&params.sender, &params.address, &params.value);

		if self.engine.is_builtin(&params.address) {
			// if destination is builtin, try to execute it
			let cost = self.engine.cost_of_builtin(&params.address, &params.data);
			match cost <= params.gas {
				true => {
					self.engine.execute_builtin(&params.address, &params.data, &mut output);
					Ok(params.gas - cost)
				},
				// just drain the whole gas
				false => Ok(U256::zero())
			}
		} else if params.code.len() > 0 {
			// if destination is a contract, do normal message call
			
			let res = {
				let mut ext = Externalities::from_executive(self, params, substate, OutputPolicy::Return(output));
				let evm = Factory::create();
				evm.exec(&params, &mut ext)
			};
			self.revert_if_needed(&res, substate, backup);
			res
		} else {
			// otherwise, nothing
			Ok(params.gas)
		}
	}
	
	/// Creates contract with given contract params.
	/// NOTE. It does not finalize the transaction (doesn't do refunds, nor suicides).
	/// Modifies the substate.
	fn create(&mut self, params: &ActionParams, substate: &mut Substate) -> evm::Result {
		// backup used in case of running out of gas
		let backup = self.state.clone();

		// at first create new contract
		self.state.new_contract(&params.address);

		// then transfer value to it
		self.state.transfer_balance(&params.sender, &params.address, &params.value);

		let res = {
			let mut ext = Externalities::from_executive(self, params, substate, OutputPolicy::InitContract);
			let evm = Factory::create();
			evm.exec(&params, &mut ext)
		};
		self.revert_if_needed(&res, substate, backup);
		res
	}

	/// Finalizes the transaction (does refunds and suicides).
	fn finalize(&mut self, t: &Transaction, substate: Substate, result: evm::Result) -> ExecutionResult {
		match result { 
			Err(evm::Error::Internal) => Err(ExecutionError::Internal),
			Ok(gas_left) => {
				let schedule = self.engine.schedule(self.info);

				// refunds from SSTORE nonzero -> zero
				let sstore_refunds = U256::from(schedule.sstore_refund_gas) * substate.refunds_count;
				// refunds from contract suicides
				let suicide_refunds = U256::from(schedule.suicide_refund_gas) * U256::from(substate.suicides.len());

				// real ammount to refund
				let refund = cmp::min(sstore_refunds + suicide_refunds, (t.gas - gas_left) / U256::from(2)) + gas_left;
				let refund_value = refund * t.gas_price;
				self.state.add_balance(&t.sender().unwrap(), &refund_value);
				
				// fees earned by author
				let fees = t.gas - refund;
				let fees_value = fees * t.gas_price;
				let author = &self.info.author;
				self.state.add_balance(author, &fees_value);

				// perform suicides
				for address in substate.suicides.iter() {
					self.state.kill_account(address);
				}

				let gas_used = t.gas - gas_left;
				Ok(Executed {
					gas: t.gas,
					gas_used: gas_used,
					refunded: refund,
					cumulative_gas_used: self.info.gas_used + gas_used,
					logs: substate.logs,
					out_of_gas: substate.out_of_gas,
					contracts_created: substate.contracts_created
				})
			},
			_err => {
				Ok(Executed {
					gas: t.gas,
					gas_used: t.gas,
					refunded: U256::zero(),
					cumulative_gas_used: self.info.gas_used + t.gas,
					logs: vec![],
					out_of_gas: true,
					contracts_created: vec![]
				})
			}
		}
	}

	fn revert_if_needed(&mut self, result: &evm::Result, substate: &mut Substate, backup: State) {
		// TODO: handle other evm::Errors same as OutOfGas once they are implemented
		match &result {
			&Err(evm::Error::OutOfGas) => {
				substate.out_of_gas = true;
				self.state.revert(backup);
			},
			&Err(evm::Error::Internal) => (),
			&Ok(_) => ()
			
		}
		result
	}
}

/// Policy for handling output data on `RETURN` opcode.
pub enum OutputPolicy<'a> {
	/// Return reference to fixed sized output.
	/// Used for message calls.
	Return(BytesRef<'a>),
	/// Init new contract as soon as `RETURN` is called.
	InitContract
}

/// Implementation of evm Externalities.
pub struct Externalities<'a> {
	#[cfg(test)]
	pub state: &'a mut State,
	#[cfg(not(test))]
	state: &'a mut State,
	info: &'a EnvInfo,
	engine: &'a Engine,
	depth: usize,
	#[cfg(test)]
	pub params: &'a ActionParams,
	#[cfg(not(test))]
	params: &'a ActionParams,
	substate: &'a mut Substate,
	schedule: Schedule,
	output: OutputPolicy<'a>
}

impl<'a> Externalities<'a> {
	/// Basic `Externalities` constructor.
	pub fn new(state: &'a mut State, 
			   info: &'a EnvInfo, 
			   engine: &'a Engine, 
			   depth: usize,
			   params: &'a ActionParams, 
			   substate: &'a mut Substate, 
			   output: OutputPolicy<'a>) -> Self {
		Externalities {
			state: state,
			info: info,
			engine: engine,
			depth: depth,
			params: params,
			substate: substate,
			schedule: engine.schedule(info),
			output: output
		}
	}

	/// Creates `Externalities` from `Executive`.
	fn from_executive(e: &'a mut Executive, params: &'a ActionParams, substate: &'a mut Substate, output: OutputPolicy<'a>) -> Self {
		Self::new(e.state, e.info, e.engine, e.depth, params, substate, output)
	}
}

impl<'a> Ext for Externalities<'a> {
	fn sload(&self, key: &H256) -> H256 {
		self.state.storage_at(&self.params.address, key)
	}

	fn sstore(&mut self, key: H256, value: H256) {
		// if SSTORE nonzero -> zero, increment refund count
		if value == H256::new() && self.state.storage_at(&self.params.address, &key) != H256::new() {
			self.substate.refunds_count = self.substate.refunds_count + U256::one();
		}
		self.state.set_storage(&self.params.address, key, value)
	}

	fn balance(&self, address: &Address) -> U256 {
		self.state.balance(address)
	}

	fn blockhash(&self, number: &U256) -> H256 {
		match *number < U256::from(self.info.number) && number.low_u64() >= cmp::max(256, self.info.number) - 256 {
			true => {
				let index = self.info.number - number.low_u64() - 1;
				self.info.last_hashes[index as usize].clone()
			},
			false => H256::from(&U256::zero()),
		}
	}

	fn create(&mut self, gas: &U256, value: &U256, code: &[u8]) -> (U256, Option<Address>) {
		// if balance is insufficient or we are to deep, return
		if self.state.balance(&self.params.address) < *value || self.depth >= self.schedule.max_depth {
			return (*gas, None);
		}

		// create new contract address
		let address = contract_address(&self.params.address, &self.state.nonce(&self.params.address));

		// prepare the params
		let params = ActionParams {
			address: address.clone(),
			sender: self.params.address.clone(),
			origin: self.params.origin.clone(),
			gas: *gas,
			gas_price: self.params.gas_price.clone(),
			value: value.clone(),
			code: code.to_vec(),
			data: vec![],
		};

		let mut ex = Executive::from_parent(self.state, self.info, self.engine, self.depth);
		ex.state.inc_nonce(&self.params.address);
		match ex.create(&params, self.substate) {
			Ok(gas_left) => (gas_left, Some(address)),
			_ => (U256::zero(), None)
		}
	}

	fn call(&mut self, 
			gas: &U256, 
			call_gas: &U256, 
			receive_address: &Address, 
			value: &U256, 
			data: &[u8], 
			code_address: &Address, 
			output: &mut [u8]) -> Result<(U256, bool), evm::Error> {
		let mut gas_cost = *call_gas;
		let mut call_gas = *call_gas;

		let is_call = receive_address == code_address;
		if is_call && !self.state.exists(&code_address) {
			gas_cost = gas_cost + U256::from(self.schedule.call_new_account_gas);
		}

		if *value > U256::zero() {
			assert!(self.schedule.call_value_transfer_gas > self.schedule.call_stipend, "overflow possible");
			gas_cost = gas_cost + U256::from(self.schedule.call_value_transfer_gas);
			call_gas = call_gas + U256::from(self.schedule.call_stipend);
		}

		if gas_cost > *gas {
			return Err(evm::Error::OutOfGas);
		}

		let gas = *gas - gas_cost;

		// if balance is insufficient or we are to deep, return
		if self.state.balance(&self.params.address) < *value || self.depth >= self.schedule.max_depth {
			return Ok((gas + call_gas, true));
		}

		let params = ActionParams {
			address: receive_address.clone(), 
			sender: self.params.address.clone(),
			origin: self.params.origin.clone(),
			gas: call_gas,
			gas_price: self.params.gas_price.clone(),
			value: value.clone(),
			code: self.state.code(code_address).unwrap_or(vec![]),
			data: data.to_vec(),
		};

		let mut ex = Executive::from_parent(self.state, self.info, self.engine, self.depth);
		match ex.call(&params, self.substate, BytesRef::Fixed(output)) {
			Ok(gas_left) => Ok((gas + gas_left, true)), //Some(CallResult::new(gas + gas_left, true)),
			_ => Ok((gas, false))
		}
	}

	fn extcode(&self, address: &Address) -> Vec<u8> {
		self.state.code(address).unwrap_or(vec![])
	}

	fn ret(&mut self, gas: &U256, data: &[u8]) -> Result<U256, evm::Error> {
		match &mut self.output {
			&mut OutputPolicy::Return(BytesRef::Fixed(ref mut slice)) => unsafe {
				let len = cmp::min(slice.len(), data.len());
				ptr::copy(data.as_ptr(), slice.as_mut_ptr(), len);
				Ok(*gas)
			},
			&mut OutputPolicy::Return(BytesRef::Flexible(ref mut vec)) => unsafe {
				vec.clear();
				vec.reserve(data.len());
				ptr::copy(data.as_ptr(), vec.as_mut_ptr(), data.len());
				vec.set_len(data.len());
				Ok(*gas)
			},
			&mut OutputPolicy::InitContract => {
				let return_cost = U256::from(data.len()) * U256::from(self.schedule.create_data_gas);
				if return_cost > *gas {
					return match self.schedule.exceptional_failed_code_deposit {
						true => Err(evm::Error::OutOfGas),
						false => Ok(*gas)
					}
				}
				let mut code = vec![];
				code.reserve(data.len());
				unsafe {
					ptr::copy(data.as_ptr(), code.as_mut_ptr(), data.len());
					code.set_len(data.len());
				}
				let address = &self.params.address;
				self.state.init_code(address, code);
				self.substate.contracts_created.push(address.clone());
				Ok(*gas - return_cost)
			}
		}
	}

	fn log(&mut self, topics: Vec<H256>, data: Bytes) {
		let address = self.params.address.clone();
		self.substate.logs.push(LogEntry::new(address, topics, data));
	}

	fn suicide(&mut self, refund_address: &Address) {
		let address = self.params.address.clone();
		let balance = self.balance(&address);
		self.state.transfer_balance(&address, refund_address, &balance);
		self.substate.suicides.insert(address);
	}

	fn schedule(&self) -> &Schedule {
		&self.schedule
	}

	fn env_info(&self) -> &EnvInfo {
		&self.info
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use common::*;
	use state::*;
	use ethereum;
	use engine::*;
	use spec::*;
	use evm::Schedule;

	struct TestEngine {
		spec: Spec,
		max_depth: usize
	}

	impl TestEngine {
		fn new(max_depth: usize) -> TestEngine {
			TestEngine {
				spec: ethereum::new_frontier_test(),
				max_depth: max_depth 
			}
		}
	}

	impl Engine for TestEngine {
		fn name(&self) -> &str { "TestEngine" }
		fn spec(&self) -> &Spec { &self.spec }
		fn schedule(&self, _env_info: &EnvInfo) -> Schedule { 
			let mut schedule = Schedule::new_frontier();
			schedule.max_depth = self.max_depth;
			schedule
		}
	}

	#[test]
	fn test_contract_address() {
		let address = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let expected_address = Address::from_str("3f09c73a5ed19289fb9bdc72f1742566df146f56").unwrap();
		assert_eq!(expected_address, contract_address(&address, &U256::from(88)));
	}

	#[test]
	// TODO: replace params with transactions!
	fn test_sender_balance() {
		let sender = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address = contract_address(&sender, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.gas = U256::from(100_000);
		params.code = "3331600055".from_hex().unwrap();
		params.value = U256::from(0x7);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(0x100u64));
		let info = EnvInfo::new();
		let engine = TestEngine::new(0);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap()
		};

		assert_eq!(gas_left, U256::from(79_975));
		assert_eq!(state.storage_at(&address, &H256::new()), H256::from(&U256::from(0xf9u64)));
		assert_eq!(state.balance(&sender), U256::from(0xf9));
		assert_eq!(state.balance(&address), U256::from(0x7));
		// 0 cause contract hasn't returned
		assert_eq!(substate.contracts_created.len(), 0);

		// TODO: just test state root.
	}

	#[test]
	fn test_create_contract() {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 17 - push 17
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d60036017f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(&sender, &U256::zero());
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = code.clone();
		params.value = U256::from(100);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100));
		let info = EnvInfo::new();
		let engine = TestEngine::new(0);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap()
		};
		
		assert_eq!(gas_left, U256::from(62_976));
		// ended with max depth
		assert_eq!(substate.contracts_created.len(), 0);
	}

	#[test]
	fn test_create_contract_value_too_high() {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 e6 - push 230
		// f0 - create a contract trying to send 230.
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d600360e6f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(&sender, &U256::zero());
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = code.clone();
		params.value = U256::from(100);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100));
		let info = EnvInfo::new();
		let engine = TestEngine::new(0);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap()
		};
		
		assert_eq!(gas_left, U256::from(62_976));
		assert_eq!(substate.contracts_created.len(), 0);
	}

	#[test]
	fn test_create_contract_without_max_depth() {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 17 - push 17
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d60036017f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(&sender, &U256::zero());
		let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = code.clone();
		params.value = U256::from(100);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100));
		let info = EnvInfo::new();
		let engine = TestEngine::new(1024);
		let mut substate = Substate::new();

		{
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap();
		}
		
		assert_eq!(substate.contracts_created.len(), 1);
		assert_eq!(substate.contracts_created[0], next_address);
	}

	#[test]
	fn test_aba_calls() {
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 18 - push 18
		// 73 945304eb96065b2a98b57a48a06ae28d285a71b5 - push this address
		// 61 03e8 - push 1000
		// f1 - message call
		// 58 - get PC
		// 55 - sstore

		let code_a = "6000600060006000601873945304eb96065b2a98b57a48a06ae28d285a71b56103e8f15855".from_hex().unwrap();

		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 17 - push 17
		// 73 0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6 - push this address
		// 61 0x01f4 - push 500
		// f1 - message call
		// 60 01 - push 1
		// 01 - add
		// 58 - get PC
		// 55 - sstore
		let code_b = "60006000600060006017730f572e5295c57f15886f9b263e2f6d2d6c7b5ec66101f4f16001015855".from_hex().unwrap();

		let address_a = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address_b = Address::from_str("945304eb96065b2a98b57a48a06ae28d285a71b5" ).unwrap();
		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();

		let mut params = ActionParams::new();
		params.address = address_a.clone();
		params.sender = sender.clone();
		params.gas = U256::from(100_000);
		params.code = code_a.clone();
		params.value = U256::from(100_000);

		let mut state = State::new_temp();
		state.init_code(&address_a, code_a.clone());
		state.init_code(&address_b, code_b.clone());
		state.add_balance(&sender, &U256::from(100_000));

		let info = EnvInfo::new();
		let engine = TestEngine::new(0);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.call(&params, &mut substate, BytesRef::Fixed(&mut [])).unwrap()
		};

		assert_eq!(gas_left, U256::from(73_237));
		assert_eq!(state.storage_at(&address_a, &H256::from(&U256::from(0x23))), H256::from(&U256::from(1)));
	}

	#[test]
	fn test_recursive_bomb1() {
		// 60 01 - push 1
		// 60 00 - push 0
		// 54 - sload 
		// 01 - add
		// 60 00 - push 0
		// 55 - sstore
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 30 - load address
		// 60 e0 - push e0
		// 5a - get gas
		// 03 - sub
		// f1 - message call (self in this case)
		// 60 01 - push 1
		// 55 - sstore
		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let code = "600160005401600055600060006000600060003060e05a03f1600155".from_hex().unwrap();
		let address = contract_address(&sender, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.gas = U256::from(100_000);
		params.code = code.clone();
		let mut state = State::new_temp();
		state.init_code(&address, code.clone());
		let info = EnvInfo::new();
		let engine = TestEngine::new(0);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.call(&params, &mut substate, BytesRef::Fixed(&mut [])).unwrap()
		};

		assert_eq!(gas_left, U256::from(59_870));
		assert_eq!(state.storage_at(&address, &H256::from(&U256::zero())), H256::from(&U256::from(1)));
		assert_eq!(state.storage_at(&address, &H256::from(&U256::one())), H256::from(&U256::from(1)));
	}

	#[test]
	fn test_transact_simple() {
		let mut t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::zero(), U256::zero());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();
		let contract = contract_address(&sender, &U256::zero());

		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(18));	
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0);

		let executed = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t).unwrap()
		};

		assert_eq!(executed.gas, U256::from(100_000));
		assert_eq!(executed.gas_used, U256::from(41_301));
		assert_eq!(executed.refunded, U256::from(58_699));
		assert_eq!(executed.cumulative_gas_used, U256::from(41_301));
		assert_eq!(executed.logs.len(), 0);
		assert_eq!(executed.out_of_gas, false);
		assert_eq!(executed.contracts_created.len(), 0);
		assert_eq!(state.balance(&sender), U256::from(1));
		assert_eq!(state.balance(&contract), U256::from(17));
		assert_eq!(state.nonce(&sender), U256::from(1));
		assert_eq!(state.storage_at(&contract, &H256::new()), H256::from(&U256::from(1)));
	}

	#[test]
	fn test_transact_invalid_sender() {
		let t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::zero(), U256::zero());

		let mut state = State::new_temp();
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};
		
		match res {
			Err(Error::Util(UtilError::Crypto(CryptoError::InvalidSignature))) => (),
			_ => assert!(false, "Expected invalid signature error.")
		}
	}

	#[test]
	fn test_transact_invalid_nonce() {
		let mut t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::zero(), U256::one());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();
		
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(17));	
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};
		
		match res {
			Err(Error::Execution(ExecutionError::InvalidNonce { expected, is })) 
				if expected == U256::zero() && is == U256::one() => (), 
			_ => assert!(false, "Expected invalid nonce error.")
		}
	}

	#[test]
	fn test_transact_gas_limit_reached() {
		let mut t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(80_001), U256::zero(), U256::zero());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();

		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(17));	
		let mut info = EnvInfo::new();
		info.gas_used = U256::from(20_000);
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};

		match res {
			Err(Error::Execution(ExecutionError::BlockGasLimitReached { gas_limit, gas_used, gas })) 
				if gas_limit == U256::from(100_000) && gas_used == U256::from(20_000) && gas == U256::from(80_001) => (), 
			_ => assert!(false, "Expected block gas limit error.")
		}
	}

	#[test]
	fn test_not_enough_cash() {
		let mut t = Transaction::new_create(U256::from(18), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::one(), U256::zero());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();

		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100_017));	
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};
		
		match res {
			Err(Error::Execution(ExecutionError::NotEnoughCash { required , is })) 
				if required == U512::from(100_018) && is == U512::from(100_017) => (), 
			_ => assert!(false, "Expected not enough cash error. {:?}", res)
		}
	}
}
