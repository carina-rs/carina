//! `cidr_subnet(prefix, newbits, netnum)` built-in function

use std::net::Ipv4Addr;

use crate::resource::Value;

use super::value_type_name;

/// `cidr_subnet(prefix, newbits, netnum)` - Calculate a subnet CIDR from a network prefix.
///
/// - `prefix`: base CIDR string (e.g., "10.0.0.0/16")
/// - `newbits`: number of additional bits for the subnet mask (Int)
/// - `netnum`: subnet number within the new address space (Int)
/// - Returns: subnet CIDR string (e.g., "10.0.1.0/24")
///
/// Examples:
/// ```text
/// cidr_subnet("10.0.0.0/16", 8, 0)  // => "10.0.0.0/24"
/// cidr_subnet("10.0.0.0/16", 8, 1)  // => "10.0.1.0/24"
/// cidr_subnet("10.0.0.0/16", 8, 255) // => "10.0.255.0/24"
/// ```
pub(crate) fn builtin_cidr_subnet(args: &[Value]) -> Result<Value, String> {
    if args.len() != 3 {
        return Err(format!(
            "cidr_subnet() expects 3 arguments (prefix, newbits, netnum), got {}",
            args.len()
        ));
    }

    let prefix_str = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(format!(
                "cidr_subnet() first argument (prefix) must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let newbits = match &args[1] {
        Value::Int(n) => *n,
        other => {
            return Err(format!(
                "cidr_subnet() second argument (newbits) must be an integer, got {}",
                value_type_name(other)
            ));
        }
    };

    let netnum = match &args[2] {
        Value::Int(n) => *n,
        other => {
            return Err(format!(
                "cidr_subnet() third argument (netnum) must be an integer, got {}",
                value_type_name(other)
            ));
        }
    };

    let result = calculate_cidr_subnet(&prefix_str, newbits, netnum)?;
    Ok(Value::String(result))
}

/// Parse a CIDR string into (Ipv4Addr, prefix_length).
fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u32), String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "cidr_subnet() invalid CIDR format: '{}' (expected 'address/prefix')",
            cidr
        ));
    }

    let addr: Ipv4Addr = parts[0]
        .parse()
        .map_err(|e| format!("cidr_subnet() invalid IP address '{}': {}", parts[0], e))?;

    let prefix_len: u32 = parts[1]
        .parse()
        .map_err(|e| format!("cidr_subnet() invalid prefix length '{}': {}", parts[1], e))?;

    if prefix_len > 32 {
        return Err(format!(
            "cidr_subnet() prefix length {} exceeds maximum of 32",
            prefix_len
        ));
    }

    Ok((addr, prefix_len))
}

/// Calculate the subnet CIDR given a base prefix, additional bits, and subnet number.
fn calculate_cidr_subnet(prefix: &str, newbits: i64, netnum: i64) -> Result<String, String> {
    if newbits < 0 {
        return Err(format!(
            "cidr_subnet() newbits must be non-negative, got {}",
            newbits
        ));
    }
    if netnum < 0 {
        return Err(format!(
            "cidr_subnet() netnum must be non-negative, got {}",
            netnum
        ));
    }

    let (addr, prefix_len) = parse_cidr(prefix)?;
    let newbits = newbits as u32;
    let netnum = netnum as u64;

    let new_prefix_len = prefix_len + newbits;
    if new_prefix_len > 32 {
        return Err(format!(
            "cidr_subnet() resulting prefix length {} ({}+{}) exceeds maximum of 32",
            new_prefix_len, prefix_len, newbits
        ));
    }

    // Maximum number of subnets with newbits additional bits
    let max_netnum: u64 = 1u64 << newbits;
    if netnum >= max_netnum {
        return Err(format!(
            "cidr_subnet() netnum {} is out of range for {} additional bits (max {})",
            netnum,
            newbits,
            max_netnum - 1
        ));
    }

    let addr_u32 = u32::from(addr);

    // Mask the base address to the network prefix
    let base_mask = if prefix_len == 0 {
        0u32
    } else {
        !0u32 << (32 - prefix_len)
    };
    let base_network = addr_u32 & base_mask;

    // Calculate the subnet offset
    let host_bits = 32 - new_prefix_len;
    let subnet_offset = (netnum as u32) << host_bits;

    let subnet_addr = base_network | subnet_offset;
    let result_addr = Ipv4Addr::from(subnet_addr);

    Ok(format!("{}/{}", result_addr, new_prefix_len))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::Value;

    fn cidr_subnet(prefix: &str, newbits: i64, netnum: i64) -> Result<Value, String> {
        let args = vec![
            Value::String(prefix.to_string()),
            Value::Int(newbits),
            Value::Int(netnum),
        ];
        evaluate_builtin("cidr_subnet", &args)
    }

    #[test]
    fn basic_subnet_calculation() {
        assert_eq!(
            cidr_subnet("10.0.0.0/16", 8, 0).unwrap(),
            Value::String("10.0.0.0/24".to_string())
        );
        assert_eq!(
            cidr_subnet("10.0.0.0/16", 8, 1).unwrap(),
            Value::String("10.0.1.0/24".to_string())
        );
        assert_eq!(
            cidr_subnet("10.0.0.0/16", 8, 2).unwrap(),
            Value::String("10.0.2.0/24".to_string())
        );
    }

    #[test]
    fn subnet_255() {
        assert_eq!(
            cidr_subnet("10.0.0.0/16", 8, 255).unwrap(),
            Value::String("10.0.255.0/24".to_string())
        );
    }

    #[test]
    fn slash_8_to_slash_16() {
        assert_eq!(
            cidr_subnet("10.0.0.0/8", 8, 0).unwrap(),
            Value::String("10.0.0.0/16".to_string())
        );
        assert_eq!(
            cidr_subnet("10.0.0.0/8", 8, 1).unwrap(),
            Value::String("10.1.0.0/16".to_string())
        );
        assert_eq!(
            cidr_subnet("10.0.0.0/8", 8, 255).unwrap(),
            Value::String("10.255.0.0/16".to_string())
        );
    }

    #[test]
    fn slash_24_to_slash_28() {
        assert_eq!(
            cidr_subnet("192.168.1.0/24", 4, 0).unwrap(),
            Value::String("192.168.1.0/28".to_string())
        );
        assert_eq!(
            cidr_subnet("192.168.1.0/24", 4, 1).unwrap(),
            Value::String("192.168.1.16/28".to_string())
        );
        assert_eq!(
            cidr_subnet("192.168.1.0/24", 4, 15).unwrap(),
            Value::String("192.168.1.240/28".to_string())
        );
    }

    #[test]
    fn slash_0_base() {
        assert_eq!(
            cidr_subnet("0.0.0.0/0", 8, 0).unwrap(),
            Value::String("0.0.0.0/8".to_string())
        );
        assert_eq!(
            cidr_subnet("0.0.0.0/0", 8, 10).unwrap(),
            Value::String("10.0.0.0/8".to_string())
        );
    }

    #[test]
    fn single_host_subnet() {
        // /24 + 8 newbits = /32
        assert_eq!(
            cidr_subnet("192.168.1.0/24", 8, 0).unwrap(),
            Value::String("192.168.1.0/32".to_string())
        );
        assert_eq!(
            cidr_subnet("192.168.1.0/24", 8, 42).unwrap(),
            Value::String("192.168.1.42/32".to_string())
        );
    }

    #[test]
    fn newbits_1_halves_network() {
        assert_eq!(
            cidr_subnet("10.0.0.0/16", 1, 0).unwrap(),
            Value::String("10.0.0.0/17".to_string())
        );
        assert_eq!(
            cidr_subnet("10.0.0.0/16", 1, 1).unwrap(),
            Value::String("10.0.128.0/17".to_string())
        );
    }

    #[test]
    fn error_prefix_overflow() {
        let result = cidr_subnet("10.0.0.0/24", 9, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds maximum of 32"));
    }

    #[test]
    fn error_netnum_out_of_range() {
        // 8 newbits => max netnum is 255
        let result = cidr_subnet("10.0.0.0/16", 8, 256);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }

    #[test]
    fn error_negative_newbits() {
        let result = cidr_subnet("10.0.0.0/16", -1, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-negative"));
    }

    #[test]
    fn error_negative_netnum() {
        let result = cidr_subnet("10.0.0.0/16", 8, -1);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-negative"));
    }

    #[test]
    fn error_invalid_cidr() {
        let result = cidr_subnet("not-a-cidr", 8, 0);
        assert!(result.is_err());
    }

    #[test]
    fn error_missing_prefix_length() {
        let result = cidr_subnet("10.0.0.0", 8, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid CIDR format"));
    }

    #[test]
    fn partial_application_with_two_args() {
        use crate::builtins::evaluate_builtin_for_tests;
        use crate::eval_value::EvalValue;
        let args = vec![Value::String("10.0.0.0/16".to_string()), Value::Int(8)];
        let result = evaluate_builtin_for_tests("cidr_subnet", &args).unwrap();
        match result {
            EvalValue::Closure {
                name,
                captured_args,
                remaining_arity,
            } => {
                assert_eq!(name, "cidr_subnet");
                assert_eq!(captured_args.len(), 2);
                assert_eq!(remaining_arity, 1);
            }
            other => panic!("Expected Closure, got {:?}", other),
        }
    }

    #[test]
    fn error_wrong_arg_types() {
        // First arg not string
        let args = vec![Value::Int(10), Value::Int(8), Value::Int(0)];
        let result = evaluate_builtin("cidr_subnet", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a string"));

        // Second arg not int
        let args = vec![
            Value::String("10.0.0.0/16".to_string()),
            Value::String("8".to_string()),
            Value::Int(0),
        ];
        let result = evaluate_builtin("cidr_subnet", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be an integer"));

        // Third arg not int
        let args = vec![
            Value::String("10.0.0.0/16".to_string()),
            Value::Int(8),
            Value::String("0".to_string()),
        ];
        let result = evaluate_builtin("cidr_subnet", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be an integer"));
    }

    #[test]
    fn error_prefix_length_too_large() {
        let result = cidr_subnet("10.0.0.0/33", 0, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds maximum of 32"));
    }

    #[test]
    fn base_address_masked_to_network() {
        // Even if the input has host bits set, they are masked off
        assert_eq!(
            cidr_subnet("10.0.0.5/16", 8, 1).unwrap(),
            Value::String("10.0.1.0/24".to_string())
        );
    }
}
