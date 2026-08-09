//! smartctl -j parsing tests.

use shr_inspect::parse_smartctl;

const ATA_WARN: &str = r#"{
  "model_name":"ST8000VN004-2M2101",
  "serial_number":"WKD1ABCD",
  "smart_status":{"passed":true},
  "temperature":{"current":43},
  "power_on_time":{"hours":2150},
  "ata_smart_attributes":{"table":[
    {"id":5,"name":"Reallocated_Sector_Ct","raw":{"value":0}},
    {"id":197,"name":"Current_Pending_Sector","raw":{"value":1}}
  ]}
}"#;

#[test]
fn parses_ata_health_and_attributes() {
    let s = parse_smartctl(ATA_WARN).unwrap();
    assert_eq!(s.passed, Some(true));
    assert_eq!(s.temperature_c, Some(43));
    assert_eq!(s.power_on_hours, Some(2150));
    assert_eq!(s.reallocated_sectors, Some(0));
    assert_eq!(s.pending_sectors, Some(1));
    assert_eq!(s.model.as_deref(), Some("ST8000VN004-2M2101"));
    assert_eq!(s.serial.as_deref(), Some("WKD1ABCD"));
}

#[test]
fn pending_sector_triggers_warning() {
    let s = parse_smartctl(ATA_WARN).unwrap();
    assert!(s.has_warning());
}

#[test]
fn healthy_disk_has_no_warning() {
    let json = r#"{"smart_status":{"passed":true},"temperature":{"current":38},
      "ata_smart_attributes":{"table":[{"id":5,"raw":{"value":0}},{"id":197,"raw":{"value":0}}]}}"#;
    let s = parse_smartctl(json).unwrap();
    assert!(!s.has_warning());
}

#[test]
fn falls_back_to_nvme_temperature() {
    let json = r#"{"smart_status":{"passed":true},
      "nvme_smart_health_information_log":{"temperature":41}}"#;
    let s = parse_smartctl(json).unwrap();
    assert_eq!(s.temperature_c, Some(41));
    assert_eq!(s.reallocated_sectors, None);
}

#[test]
fn failed_health_assessment_warns() {
    let s = parse_smartctl(r#"{"smart_status":{"passed":false}}"#).unwrap();
    assert_eq!(s.passed, Some(false));
    assert!(s.has_warning());
    assert!(!s.is_unknown());
}

#[test]
fn exit_status_problem_bits_warn_but_usage_errors_are_unknown() {
    // bit 3 (value 8) = disk failing: a real warning, not "unknown".
    let bad = parse_smartctl(r#"{"smartctl":{"exit_status":8}}"#).unwrap();
    assert!(bad.has_warning());
    assert!(!bad.is_unknown());
    // bit 1 (value 2) = device open failed: inspection failed → unknown, not clean.
    let usage = parse_smartctl(r#"{"smartctl":{"exit_status":2}}"#).unwrap();
    assert!(!usage.has_warning());
    assert!(usage.is_unknown());
}

#[test]
fn nvme_and_ata_error_signals_warn_and_are_not_unknown() {
    let nvme_crit =
        parse_smartctl(r#"{"nvme_smart_health_information_log":{"critical_warning":8,"temperature":41}}"#)
            .unwrap();
    assert!(nvme_crit.has_warning());
    assert!(!nvme_crit.is_unknown());

    let nvme_media = parse_smartctl(r#"{"nvme_smart_health_information_log":{"media_errors":5}}"#).unwrap();
    assert_eq!(nvme_media.nvme_media_errors, Some(5));
    assert!(nvme_media.has_warning());
    assert!(!nvme_media.is_unknown());

    // ATA 198 uncorrectable, without a passed verdict, is a warning (and known).
    let ata = parse_smartctl(r#"{"ata_smart_attributes":{"table":[{"id":198,"raw":{"value":3}}]}}"#).unwrap();
    assert_eq!(ata.uncorrectable_sectors, Some(3));
    assert!(ata.has_warning());
    assert!(!ata.is_unknown());
}

#[test]
fn empty_smart_is_unknown_not_a_warning() {
    let s = parse_smartctl("{}").unwrap();
    assert!(s.is_unknown());
    assert!(!s.has_warning());
}
