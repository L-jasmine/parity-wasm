
use elements::{
	deserialize_buffer, GlobalType,
	ValueType, TableType, MemoryType, FunctionType,
};
use validation::{validate_module, ValidatedModule};
use interpreter::{
	Error, MemoryInstance, ModuleInstance, RuntimeValue,
	HostError, MemoryRef, ImportsBuilder, Externals, TryInto, TableRef,
	GlobalRef, FuncRef, FuncInstance, ModuleImportResolver, ModuleRef,
};
use wabt::wat2wasm;

#[derive(Debug, Clone, PartialEq)]
struct HostErrorWithCode {
	error_code: u32,
}

impl ::std::fmt::Display for HostErrorWithCode {
	fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
		write!(f, "{}", self.error_code)
	}
}

impl HostError for HostErrorWithCode {}

/// Host state for the test environment.
///
/// This struct can be used as an external function executor and
/// as imports provider. This has a drawback: this struct
/// should be provided upon an instantiation of the module.
///
/// However, this limitation can be lifted by implementing `Externals`
/// and `ModuleImportResolver` traits for different structures.
struct TestHost {
	memory: Option<MemoryRef>,
	instance: Option<ModuleRef>,
}

impl TestHost {
	fn new() -> TestHost {
		TestHost {
			memory: Some(MemoryInstance::alloc(1, Some(1)).unwrap()),
			instance: None,
		}
	}
}

/// sub(a: i32, b: i32) -> i32
///
/// This function just substracts one integer from another,
/// returning the subtraction result.
const SUB_FUNC_INDEX: usize = 0;

/// err(error_code: i32) -> !
///
/// This function traps upon a call.
/// The trap have a special type - HostErrorWithCode.
const ERR_FUNC_INDEX: usize = 1;

/// inc_mem(ptr: *mut u8)
///
/// Increments value at the given address in memory. This function
/// requires attached memory.
const INC_MEM_FUNC_INDEX: usize = 2;

/// get_mem(ptr: *mut u8) -> u8
///
/// Returns value at the given address in memory. This function
/// requires attached memory.
const GET_MEM_FUNC_INDEX: usize = 3;

/// recurse<T>(val: T) -> T
///
/// If called, resolves exported function named 'recursive' from the attached
/// module instance and then calls into it with the provided argument.
/// Note that this function is polymorphic over type T.
/// This function requires attached module instance.
const RECURSE_FUNC_INDEX: usize = 4;

impl Externals for TestHost {
	fn invoke_index(
		&mut self,
		index: usize,
		args: &[RuntimeValue],
	) -> Result<Option<RuntimeValue>, Error> {
		match index {
			SUB_FUNC_INDEX => {
				let mut args = args.iter();
				let a: i32 = args.next().unwrap().try_into().unwrap();
				let b: i32 = args.next().unwrap().try_into().unwrap();

				let result: RuntimeValue = (a - b).into();

				Ok(Some(result))
			}
			ERR_FUNC_INDEX => {
				let mut args = args.iter();
				let error_code: u32 = args.next().unwrap().try_into().unwrap();
				let error = HostErrorWithCode { error_code };
				Err(Error::Host(Box::new(error)))
			}
			INC_MEM_FUNC_INDEX => {
				let mut args = args.iter();
				let ptr: u32 = args.next().unwrap().try_into().unwrap();

				let memory = self.memory.as_ref()
					.expect("Function 'inc_mem' expects attached memory");
				let mut buf = [0u8; 1];
				memory.get_into(ptr, &mut buf).unwrap();
				buf[0] += 1;
				memory.set(ptr, &buf).unwrap();

				Ok(None)
			}
			GET_MEM_FUNC_INDEX => {
				let mut args = args.iter();
				let ptr: u32 = args.next().unwrap().try_into().unwrap();

				let memory = self.memory.as_ref()
					.expect("Function 'get_mem' expects attached memory");
				let mut buf = [0u8; 1];
				memory.get_into(ptr, &mut buf).unwrap();

				Ok(Some(RuntimeValue::I32(buf[0] as i32)))
			}
			RECURSE_FUNC_INDEX => {
				let mut args = args.iter().cloned();
				let val: RuntimeValue = args.next().unwrap();

				let instance = self.instance
					.as_ref()
					.expect("Function 'recurse' expects attached module instance")
					.clone();
				let result = instance
					.invoke_export("recursive", &[val.into()], self)
					.expect("Failed to call 'recursive'")
					.expect("expected to be Some");

				if val.value_type() != result.value_type() {
					return Err(Error::Host(Box::new(HostErrorWithCode { error_code: 123 })));
				}
				Ok(Some(result))
			}
			_ => panic!("SpecModule doesn't provide function at index {}", index),
		}
	}

	fn check_signature(&self, index: usize, func_type: &FunctionType) -> bool {
		if index == RECURSE_FUNC_INDEX {
			// This function requires special handling because it is polymorphic.
			if func_type.params().len() != 1 {
				return false;
			}
			let param_type = func_type.params()[0];
			return func_type.return_type() == Some(param_type);
		}

		let (params, ret_ty): (&[ValueType], Option<ValueType>) = match index {
			SUB_FUNC_INDEX => (&[ValueType::I32, ValueType::I32], Some(ValueType::I32)),
			ERR_FUNC_INDEX => (&[ValueType::I32], None),
			INC_MEM_FUNC_INDEX => (&[ValueType::I32], None),
			GET_MEM_FUNC_INDEX => (&[ValueType::I32], Some(ValueType::I32)),
			_ => return false,
		};

		func_type.params() == params && func_type.return_type() == ret_ty
	}
}

impl ModuleImportResolver for TestHost {
	fn resolve_func(&self, field_name: &str, func_type: &FunctionType) -> Result<FuncRef, Error> {
		let index = match field_name {
			"sub" => SUB_FUNC_INDEX,
			"err" => ERR_FUNC_INDEX,
			"inc_mem" => INC_MEM_FUNC_INDEX,
			"get_mem" => GET_MEM_FUNC_INDEX,
			"recurse" => RECURSE_FUNC_INDEX,
			_ => {
				return Err(Error::Instantiation(
					format!("Export {} not found", field_name),
				))
			}
		};

		if !self.check_signature(index, func_type) {
			return Err(Error::Instantiation(format!(
				"Export `{}` doesnt match expected type {:?}",
				field_name,
				func_type
			)));
		}

		Ok(FuncInstance::alloc_host(func_type.clone(), index))
	}

	fn resolve_global(
		&self,
		field_name: &str,
		_global_type: &GlobalType,
	) -> Result<GlobalRef, Error> {
		Err(Error::Instantiation(
			format!("Export {} not found", field_name),
		))
	}

	fn resolve_memory(
		&self,
		field_name: &str,
		_memory_type: &MemoryType,
	) -> Result<MemoryRef, Error> {
		Err(Error::Instantiation(
			format!("Export {} not found", field_name),
		))
	}

	fn resolve_table(&self, field_name: &str, _table_type: &TableType) -> Result<TableRef, Error> {
		Err(Error::Instantiation(
			format!("Export {} not found", field_name),
		))
	}
}

fn parse_wat(source: &str) -> ValidatedModule {
	let wasm_binary = wat2wasm(source).expect("Failed to parse wat source");
	let module = deserialize_buffer(&wasm_binary).expect("Failed to deserialize module");
	let validated_module = validate_module(module).expect("Failed to validate module");
	validated_module
}

#[test]
fn call_host_func() {
	let module = parse_wat(
		r#"
(module
	(import "env" "sub" (func $sub (param i32 i32) (result i32)))

	(func (export "test") (result i32)
		(call $sub
			(i32.const 5)
			(i32.const 7)
		)
	)
)
"#,
	);

	let mut env = TestHost::new();

	let instance = ModuleInstance::new(
		&module,
		&ImportsBuilder::default().with_resolver("env", &env),
	).expect("Failed to instantiate module")
		.assert_no_start();

	assert_eq!(
		instance
			.invoke_export("test", &[], &mut env)
			.expect("Failed to invoke 'test' function"),
		Some(RuntimeValue::I32(-2))
	);
}

#[test]
fn host_err() {
	let module = parse_wat(
		r#"
(module
	(import "env" "err" (func $err (param i32)))

	(func (export "test")
		(call $err
			(i32.const 228)
		)
	)
)
"#,
	);

	let mut env = TestHost::new();

	let instance = ModuleInstance::new(
		&module,
		&ImportsBuilder::default().with_resolver("env", &env),
	).expect("Failed to instantiate module")
		.assert_no_start();

	let error = instance
		.invoke_export("test", &[], &mut env)
		.expect_err("`test` expected to return error");

	let host_error: Box<HostError> = match error {
		Error::Host(err) => err,
		err => panic!("Unexpected error {:?}", err),
	};

	let error_with_code = host_error
		.downcast_ref::<HostErrorWithCode>()
		.expect("Failed to downcast to expected error type");
	assert_eq!(error_with_code.error_code, 228);
}

#[test]
fn modify_mem_with_host_funcs() {
	let module = parse_wat(
	r#"
(module
	(import "env" "inc_mem" (func $inc_mem (param i32)))
	;; (import "env" "get_mem" (func $get_mem (param i32) (result i32)))

	(func (export "modify_mem")
		;; inc memory at address 12 for 4 times.
		(call $inc_mem (i32.const 12))
		(call $inc_mem (i32.const 12))
		(call $inc_mem (i32.const 12))
		(call $inc_mem (i32.const 12))
	)
)
"#,
	);

	let mut env = TestHost::new();

	let instance = ModuleInstance::new(
		&module,
		&ImportsBuilder::default().with_resolver("env", &env),
	).expect("Failed to instantiate module")
		.assert_no_start();

	instance
		.invoke_export("modify_mem", &[], &mut env)
		.expect("Failed to invoke 'test' function");

	// Check contents of memory at address 12.
	let mut buf = [0u8; 1];
	env.memory.unwrap().get_into(12, &mut buf).unwrap();

	assert_eq!(&buf, &[4]);
}

#[test]
fn pull_internal_mem_from_module() {
	let module = parse_wat(
	r#"
(module
	(import "env" "inc_mem" (func $inc_mem (param i32)))
	(import "env" "get_mem" (func $get_mem (param i32) (result i32)))

	;; declare internal memory and export it under name "mem"
	(memory (export "mem") 1 1)

	(func (export "test") (result i32)
		;; Increment value at address 1337
		(call $inc_mem (i32.const 1337))

		;; Return value at address 1337
		(call $get_mem (i32.const 1337))
	)
)
"#,
	);

	let mut env = TestHost {
		memory: None,
		instance: None,
	};

	let instance = ModuleInstance::new(
		&module,
		&ImportsBuilder::default().with_resolver("env", &env),
	).expect("Failed to instantiate module")
		.assert_no_start();

	// Get memory instance exported by name 'mem' from the module instance.
	let internal_mem = instance
		.export_by_name("mem")
		.expect("Module expected to have 'mem' export")
		.as_memory()
		.expect("'mem' export should be a memory");

	env.memory = Some(internal_mem);

	assert_eq!(
		instance.invoke_export("test", &[], &mut env).unwrap(),
		Some(RuntimeValue::I32(1))
	);
}

#[test]
fn recursion() {
	let module = parse_wat(
		r#"
(module
	;; Import 'recurse' function. Upon a call it will call back inside
	;; this module, namely to function 'recursive' defined below.
	(import "env" "recurse" (func $recurse (param i64) (result i64)))

	;; Note that we import same function but with different type signature
	;; this is possible since 'recurse' is a host function and it is defined
	;; to be polymorphic.
	(import "env" "recurse" (func (param f32) (result f32)))

	(func (export "recursive") (param i64) (result i64)
		;; return arg_0 + 42;
		(i64.add
			(get_local 0)
			(i64.const 42)
		)
	)

	(func (export "test") (result i64)
		(call $recurse (i64.const 321))
	)
)
"#,
	);

	let mut env = TestHost::new();

	let instance = ModuleInstance::new(
		&module,
		&ImportsBuilder::default().with_resolver("env", &env),
	).expect("Failed to instantiate module")
		.assert_no_start();

	// Put instance into the env, because $recurse function expects
	// attached module instance.
	env.instance = Some(instance.clone());

	assert_eq!(
		instance
			.invoke_export("test", &[], &mut env)
			.expect("Failed to invoke 'test' function"),
		// 363 = 321 + 42
		Some(RuntimeValue::I64(363))
	);
}
