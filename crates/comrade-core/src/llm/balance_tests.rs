use super::*;

#[test]
fn parses_deepseek_balance() {
    let json = r#"{
          "is_available": true,
          "balance_infos": [
            { "currency": "CNY", "total_balance": "110.50", "granted_balance": "10.00", "topped_up_balance": "100.50" }
          ]
        }"#;
    assert_eq!(parse_balance(json).as_deref(), Some("110.50 CNY"));
    assert_eq!(parse_balance("{}"), None);
}
