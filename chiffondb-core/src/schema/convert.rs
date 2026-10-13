//! Value conversion for `Database::change_*_field_type`.

use serde_json::{Number, Value};

use crate::schema::ast::TypeExpr;

/// Resolves the name of a type a field can be converted to.
pub fn scalar_type_from_name(name: &str) -> Option<TypeExpr> {
    match name {
        "String" => Some(TypeExpr::String),
        "Int" => Some(TypeExpr::Int),
        "Float" => Some(TypeExpr::Float),
        "Boolean" => Some(TypeExpr::Boolean),
        "DateTime" => Some(TypeExpr::DateTime),
        _ => None,
    }
}

/// Whether a field declared as `from` may be changed to `to`: any pair with `String` on one
/// side among the scalar types, plus `Int` ↔ `Float`.
pub fn conversion_supported(from: &TypeExpr, to: &TypeExpr) -> bool {
    let scalar = |t: &TypeExpr| {
        matches!(
            t,
            TypeExpr::String
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Boolean
                | TypeExpr::DateTime
        )
    };
    if !scalar(from) || !scalar(to) {
        return false;
    }
    matches!(
        (from, to),
        (TypeExpr::String, _)
            | (_, TypeExpr::String)
            | (TypeExpr::Int, TypeExpr::Float)
            | (TypeExpr::Float, TypeExpr::Int)
    )
}

/// Converts a stored value to the scalar type `to`, or returns `None` when it has no exact
/// counterpart. The conversion is driven by the value's own JSON kind, so a value that already
/// fits `to` is returned as is. `null` is not handled here: callers leave it untouched.
///
/// Conversions are strict so that none of them loses information silently: a string must be
/// the whole literal (surrounding whitespace aside), `"1.0"` is not an `Int`, and a `Float`
/// becomes an `Int` only when it has no fractional part.
pub fn convert_value(value: &Value, to: &TypeExpr) -> Option<Value> {
    match (to, value) {
        (TypeExpr::String, Value::String(_)) => Some(value.clone()),
        (TypeExpr::String, Value::Number(n)) => Some(Value::String(n.to_string())),
        (TypeExpr::String, Value::Bool(b)) => Some(Value::String(b.to_string())),

        (TypeExpr::Int, Value::Number(n)) => {
            if n.is_i64() {
                Some(value.clone())
            } else {
                float_to_int(n.as_f64()?)
            }
        }
        (TypeExpr::Int, Value::String(s)) => s.trim().parse::<i64>().ok().map(Value::from),

        (TypeExpr::Float, Value::Number(n)) => {
            if n.is_f64() {
                Some(value.clone())
            } else {
                int_to_float(n.as_i64()?)
            }
        }
        (TypeExpr::Float, Value::String(s)) => {
            // `from_f64` rejects the non-finite values `parse` accepts ("inf", "NaN").
            let f = s.trim().parse::<f64>().ok()?;
            Number::from_f64(f).map(Value::Number)
        }

        (TypeExpr::Boolean, Value::Bool(_)) => Some(value.clone()),
        (TypeExpr::Boolean, Value::String(s)) => match s.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },

        (TypeExpr::DateTime, Value::String(_)) => Some(value.clone()),

        _ => None,
    }
}

fn float_to_int(f: f64) -> Option<Value> {
    // 2^63 itself is out of range; `-2^63` is `i64::MIN` and representable.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if f.fract() != 0.0 || !(-LIMIT..LIMIT).contains(&f) {
        return None;
    }
    Some(Value::from(f as i64))
}

fn int_to_float(i: i64) -> Option<Value> {
    let f = i as f64;
    // Beyond 2^53 not every integer has an f64; compare in i128 because `f as i64` saturates.
    if f as i128 != i128::from(i) {
        return None;
    }
    Number::from_f64(f).map(Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_to_int() {
        let to = TypeExpr::Int;
        assert_eq!(convert_value(&json!("30"), &to), Some(json!(30)));
        assert_eq!(convert_value(&json!("  -7\n"), &to), Some(json!(-7)));
        assert_eq!(convert_value(&json!("1.0"), &to), None);
        assert_eq!(convert_value(&json!("三十"), &to), None);
        assert_eq!(convert_value(&json!(""), &to), None);
        assert_eq!(convert_value(&json!("9223372036854775808"), &to), None);
    }

    #[test]
    fn string_to_float() {
        let to = TypeExpr::Float;
        assert_eq!(convert_value(&json!("1.5"), &to), Some(json!(1.5)));
        assert_eq!(convert_value(&json!(" 2 "), &to), Some(json!(2.0)));
        assert_eq!(convert_value(&json!("inf"), &to), None);
        assert_eq!(convert_value(&json!("NaN"), &to), None);
        assert_eq!(convert_value(&json!("abc"), &to), None);
    }

    #[test]
    fn string_to_boolean() {
        let to = TypeExpr::Boolean;
        assert_eq!(convert_value(&json!("true"), &to), Some(json!(true)));
        assert_eq!(convert_value(&json!(" false "), &to), Some(json!(false)));
        assert_eq!(convert_value(&json!("True"), &to), None);
        assert_eq!(convert_value(&json!("1"), &to), None);
    }

    #[test]
    fn scalars_to_string() {
        let to = TypeExpr::String;
        assert_eq!(convert_value(&json!(30), &to), Some(json!("30")));
        assert_eq!(convert_value(&json!(1.5), &to), Some(json!("1.5")));
        assert_eq!(convert_value(&json!(1.0), &to), Some(json!("1.0")));
        assert_eq!(convert_value(&json!(true), &to), Some(json!("true")));
        assert_eq!(convert_value(&json!(false), &to), Some(json!("false")));
        assert_eq!(convert_value(&json!([1]), &to), None);
    }

    #[test]
    fn float_to_int_requires_whole_number_in_range() {
        let to = TypeExpr::Int;
        assert_eq!(convert_value(&json!(3.0), &to), Some(json!(3)));
        assert_eq!(convert_value(&json!(-3.0), &to), Some(json!(-3)));
        assert_eq!(convert_value(&json!(3.5), &to), None);
        assert_eq!(convert_value(&json!(1e19), &to), None);
        assert_eq!(convert_value(&json!(7), &to), Some(json!(7)));
        assert_eq!(
            convert_value(&json!(-9_223_372_036_854_775_808.0), &to),
            Some(json!(i64::MIN))
        );
    }

    #[test]
    fn int_to_float_requires_exact_representation() {
        let to = TypeExpr::Float;
        let converted = convert_value(&json!(3), &to).unwrap();
        assert!(converted.as_number().unwrap().is_f64());
        assert_eq!(converted, json!(3.0));
        assert_eq!(convert_value(&json!(9_007_199_254_740_993_i64), &to), None);
        assert_eq!(convert_value(&json!(i64::MAX), &to), None);
        assert_eq!(convert_value(&json!(2.5), &to), Some(json!(2.5)));
    }

    #[test]
    fn boolean_and_numbers_do_not_convert() {
        assert_eq!(convert_value(&json!(true), &TypeExpr::Int), None);
        assert_eq!(convert_value(&json!(1), &TypeExpr::Boolean), None);
        assert_eq!(convert_value(&json!(1), &TypeExpr::DateTime), None);
    }

    #[test]
    fn supported_pairs() {
        use TypeExpr::*;
        assert!(conversion_supported(&String, &Int));
        assert!(conversion_supported(&Boolean, &String));
        assert!(conversion_supported(&Int, &Float));
        assert!(conversion_supported(&String, &DateTime));
        assert!(!conversion_supported(&Boolean, &Int));
        assert!(!conversion_supported(&DateTime, &Int));
        assert!(!conversion_supported(&Json, &String));
        assert!(!conversion_supported(&String, &List(Box::new(String))));
    }

    #[test]
    fn type_names() {
        assert_eq!(scalar_type_from_name("Int"), Some(TypeExpr::Int));
        assert_eq!(scalar_type_from_name("Json"), None);
        assert_eq!(scalar_type_from_name("int"), None);
    }
}
