//! JUnit XML 输出（同步 go-ios `aeda162`/`5e61501`/`d101593`）。
//!
//! 纯格式化器：只读取已收集的 [`XCTestEvent`] 流，不涉及任何设备协议。
//! 状态映射与 go-ios `convertCase` 一致：
//! - `passed` → 纯 `<testcase>`（即成功）；
//! - `failed` → `<failure message>`（正文 `file:line`）；
//! - `stalled` → `<error>`；
//! - `expected failure` → `<skipped message="expected failure[: msg]">`；
//! - 无最终状态（app 崩溃/连接丢失，用例开了没结果）→ `<error>` 而非
//!   `<skipped>`，CI 不能把崩溃跑成绿；
//! - 其余状态（XCTSkip 类）→ `<skipped message>`。
//!
//! 时间格式：`<time>` 为三位小数秒；`<timestamp>` 为 UTC `YYYY-MM-DDTHH:MM:SS`
//! （JUnit 惯例的 ISO8601 本地时间格式）；套件无时长时回退为用例时长之和。

use crate::model::{XCTestEvent, XCTestEventKind};

/// 单个测试用例的聚合结果。
struct JunitCase {
    suite: String,
    classname: String,
    name: String,
    started_at_ms: u64,
    duration: f64,
    /// 空串表示用例开了但从未上报最终状态（never-finished）。
    status: String,
    failure_message: Option<String>,
    failure_location: Option<String>,
}

/// 从事件流聚合测试用例结果（go-ios 由 testmanagerd 结构直读；rust 从事件流重建）。
pub fn junit_xml(events: &[XCTestEvent]) -> String {
    let mut cases: Vec<JunitCase> = Vec::new();
    let mut current_suite = String::new();
    for event in events {
        match &event.event {
            XCTestEventKind::SuiteStarted { suite, .. } => {
                current_suite = suite.clone();
            }
            XCTestEventKind::SuiteFinished { suite, .. } => {
                if current_suite == *suite {
                    current_suite = String::new();
                }
            }
            XCTestEventKind::CaseStarted { test_class, method } => {
                cases.push(JunitCase {
                    suite: if current_suite.is_empty() {
                        event.target_name.clone()
                    } else {
                        current_suite.clone()
                    },
                    classname: test_class.clone(),
                    name: method.clone(),
                    started_at_ms: event.timestamp_ms,
                    duration: 0.0,
                    status: String::new(),
                    failure_message: None,
                    failure_location: None,
                });
            }
            XCTestEventKind::CaseFinished {
                test_class,
                method,
                status,
                duration,
            } => {
                if let Some(case) = find_case(&mut cases, &current_suite, test_class, method) {
                    case.status = status.clone();
                    case.duration = *duration;
                }
            }
            XCTestEventKind::CaseFailed {
                test_class,
                method,
                message,
                file,
                line,
            } => {
                if let Some(case) = find_case(&mut cases, &current_suite, test_class, method) {
                    case.failure_message = Some(message.clone());
                    if !file.is_empty() {
                        case.failure_location = Some(format!("{file}:{line}"));
                    }
                }
            }
            XCTestEventKind::CaseStalled {
                test_class,
                method,
                file,
                line,
            } => {
                if let Some(case) = find_case(&mut cases, &current_suite, test_class, method) {
                    case.status = "stalled".into();
                    if !file.is_empty() {
                        case.failure_location = Some(format!("{file}:{line}"));
                    }
                }
            }
            _ => {}
        }
    }

    // 按套件分组（保持首次出现顺序）。
    let mut suite_order: Vec<String> = Vec::new();
    let mut grouped: Vec<(String, Vec<&JunitCase>)> = Vec::new();
    for case in &cases {
        if let Some(slot) = grouped.iter_mut().find(|(name, _)| *name == case.suite) {
            slot.1.push(case);
        } else {
            suite_order.push(case.suite.clone());
            grouped.push((case.suite.clone(), vec![case]));
        }
    }

    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let total_tests: usize = grouped.iter().map(|(_, cases)| cases.len()).sum();
    let mut total_failures = 0_usize;
    let mut total_errors = 0_usize;
    let mut total_skipped = 0_usize;
    let mut total_time = 0.0_f64;
    out.push_str("<testsuites");
    for (name, suite_cases) in &grouped {
        let suite = render_suite(name, suite_cases);
        total_failures += suite.failures;
        total_errors += suite.errors;
        total_skipped += suite.skipped;
        total_time += suite.time;
    }
    out.push_str(&format!(
        " tests=\"{total_tests}\" failures=\"{total_failures}\" errors=\"{total_errors}\" skipped=\"{total_skipped}\" time=\"{}\"",
        format_seconds(total_time)
    ));
    out.push_str(">\n");
    for (name, suite_cases) in &grouped {
        let suite = render_suite(name, suite_cases);
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{}\"",
            escape_xml(name),
            suite_cases.len(),
            suite.failures,
            suite.errors,
            suite.skipped,
            format_seconds(suite.time)
        ));
        // 时间戳取套件首个用例的开始时间。
        if let Some(first) = suite_cases.first() {
            out.push_str(&format!(
                " timestamp=\"{}\"",
                format_utc_timestamp(first.started_at_ms)
            ));
        }
        out.push_str(">\n");
        for case in suite_cases {
            out.push_str(&render_case(case));
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

struct RenderedSuite {
    failures: usize,
    errors: usize,
    skipped: usize,
    time: f64,
}

fn render_suite(name: &str, cases: &[&JunitCase]) -> RenderedSuite {
    let _ = name;
    let mut rendered = RenderedSuite {
        failures: 0,
        errors: 0,
        skipped: 0,
        time: 0.0,
    };
    for case in cases {
        rendered.time += case.duration;
        match case_element_kind(case) {
            CaseElement::Failure => rendered.failures += 1,
            CaseElement::Error => rendered.errors += 1,
            CaseElement::Skipped => rendered.skipped += 1,
            CaseElement::Plain => {}
        }
    }
    rendered
}

enum CaseElement {
    Plain,
    Failure,
    Error,
    Skipped,
}

fn case_element_kind(case: &JunitCase) -> CaseElement {
    if case.status.is_empty() {
        // 用例开了但从未上报最终状态：app 崩溃/连接丢失 → error，不是 skipped。
        return CaseElement::Error;
    }
    match case.status.to_ascii_lowercase().as_str() {
        "passed" => CaseElement::Plain,
        "failed" => CaseElement::Failure,
        "stalled" => CaseElement::Error,
        "expected failure" | "expected_failure" => CaseElement::Skipped,
        // 其余状态（XCTSkip 类）→ skipped。
        _ => CaseElement::Skipped,
    }
}

fn render_case(case: &JunitCase) -> String {
    let mut out = format!(
        "    <testcase classname=\"{}\" name=\"{}\" time=\"{}\"",
        escape_xml(&case.classname),
        escape_xml(&case.name),
        format_seconds(case.duration)
    );
    match case_element_kind(case) {
        CaseElement::Plain => {
            out.push_str("/>\n");
        }
        CaseElement::Failure => {
            let message = case
                .failure_message
                .clone()
                .unwrap_or_else(|| case.status.clone());
            out.push_str(&format!(
                ">\n      <failure message=\"{}\"",
                escape_xml(&message)
            ));
            if let Some(location) = &case.failure_location {
                out.push_str(&format!(">{}</failure>\n", escape_xml(location)));
            } else {
                out.push_str("/>\n");
            }
            out.push_str("    </testcase>\n");
        }
        CaseElement::Error => {
            let message = if case.status.is_empty() {
                case.failure_message
                    .clone()
                    .unwrap_or_else(|| "no test result received".into())
            } else {
                case.failure_message
                    .clone()
                    .unwrap_or_else(|| case.status.clone())
            };
            out.push_str(&format!(
                ">\n      <error message=\"{}\"",
                escape_xml(&message)
            ));
            if let Some(location) = &case.failure_location {
                out.push_str(&format!(">{}</error>\n", escape_xml(location)));
            } else {
                out.push_str("/>\n");
            }
            out.push_str("    </testcase>\n");
        }
        CaseElement::Skipped => {
            let message = match case.status.to_ascii_lowercase().as_str() {
                "expected failure" | "expected_failure" => {
                    let base = "expected failure".to_string();
                    match &case.failure_message {
                        Some(message) => format!("{base}: {message}"),
                        None => base,
                    }
                }
                _ => case
                    .failure_message
                    .clone()
                    .unwrap_or_else(|| case.status.clone()),
            };
            out.push_str(&format!(
                ">\n      <skipped message=\"{}\"/>\n    </testcase>\n",
                escape_xml(&message)
            ));
        }
    }
    out
}

/// `<time>` 三位小数秒（go-ios `formatSeconds`）。
fn format_seconds(seconds: f64) -> String {
    format!("{seconds:.3}")
}

/// UTC `YYYY-MM-DDTHH:MM:SS`（无 chrono 依赖的纯整数换算，proleptic Gregorian）。
fn format_utc_timestamp(timestamp_ms: u64) -> String {
    let seconds = timestamp_ms / 1000;
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
}

/// 天数（自 1970-01-01）转公历日期（Howard Hinnant 的 civil_from_days 算法）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(character),
        }
    }
    out
}

fn find_case<'a>(
    cases: &'a mut [JunitCase],
    current_suite: &str,
    test_class: &str,
    method: &str,
) -> Option<&'a mut JunitCase> {
    cases.iter_mut().rev().find(|case| {
        case.name == method
            && case.classname == test_class
            && (current_suite.is_empty() || case.suite == current_suite)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::XCTestEvent;

    fn event(kind: XCTestEventKind) -> XCTestEvent {
        XCTestEvent {
            timestamp_ms: 1_700_000_000_000,
            target_index: 0,
            target_name: "KitchenSink".into(),
            event: kind,
        }
    }

    fn sample_events() -> Vec<XCTestEvent> {
        vec![
            event(XCTestEventKind::SuiteStarted {
                suite: "KitchenSinkTests.OutcomeTests".into(),
                started_at: "2026-11-10 09:00:00 +0800".into(),
            }),
            event(XCTestEventKind::CaseStarted {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testPasses".into(),
            }),
            event(XCTestEventKind::CaseFinished {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testPasses".into(),
                status: "passed".into(),
                duration: 0.5,
            }),
            event(XCTestEventKind::CaseStarted {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testFails".into(),
            }),
            event(XCTestEventKind::CaseFailed {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testFails".into(),
                message: "XCTAssertEqual failed".into(),
                file: "OutcomeTests.swift".into(),
                line: 42,
            }),
            event(XCTestEventKind::CaseFinished {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testFails".into(),
                status: "failed".into(),
                duration: 0.25,
            }),
            event(XCTestEventKind::CaseStarted {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testSkips".into(),
            }),
            event(XCTestEventKind::CaseFinished {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testSkips".into(),
                status: "skipped".into(),
                duration: 0.1,
            }),
            event(XCTestEventKind::CaseStarted {
                test_class: "KitchenSinkTests.OutcomeTests".into(),
                method: "testNeverFinishes".into(),
            }),
            event(XCTestEventKind::SuiteFinished {
                suite: "KitchenSinkTests.OutcomeTests".into(),
                finished_at: "2026-11-10 09:00:05 +0800".into(),
                run_count: 4,
                failures: 1,
                unexpected: 0,
                test_duration: 0.85,
                total_duration: 5.0,
                skipped: 1,
                expected_failures: 0,
                uncaught_exceptions: 0,
            }),
        ]
    }

    #[test]
    fn renders_pass_failure_skip_and_never_finished() {
        let mut events = sample_events();
        // 从未完成的用例：事件流结束也没有 CaseFinished。
        let xml = junit_xml(&events);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<testsuites tests=\"4\" failures=\"1\" errors=\"1\" skipped=\"1\""));
        assert!(xml.contains("classname=\"KitchenSinkTests.OutcomeTests\" name=\"testPasses\""));
        assert!(xml.contains(
            "<failure message=\"XCTAssertEqual failed\">OutcomeTests.swift:42</failure>"
        ));
        assert!(xml.contains("<skipped message=\"skipped\"/>"));
        // never-finished 必须是 error 而非 skipped（CI 不能把崩溃跑成绿）。
        assert!(xml.contains("<error message=\"no test result received\"/>"));
        // 套件无时长回退为用例时长之和：0.5+0.25+0.1+0 = 0.85。
        assert!(xml.contains("time=\"0.850\""));
        let _ = &mut events;
    }

    #[test]
    fn maps_expected_failure_to_skipped() {
        let events = vec![
            event(XCTestEventKind::CaseStarted {
                test_class: "T".into(),
                method: "testExpectedFailure".into(),
            }),
            event(XCTestEventKind::CaseFailed {
                test_class: "T".into(),
                method: "testExpectedFailure".into(),
                message: "deliberate".into(),
                file: "T.swift".into(),
                line: 7,
            }),
            event(XCTestEventKind::CaseFinished {
                test_class: "T".into(),
                method: "testExpectedFailure".into(),
                status: "expected failure".into(),
                duration: 0.2,
            }),
        ];
        let xml = junit_xml(&events);
        assert!(xml.contains("<skipped message=\"expected failure: deliberate\"/>"));
        assert!(xml.contains("skipped=\"1\""));
    }

    #[test]
    fn maps_stalled_to_error_and_escapes_xml() {
        let events = vec![
            event(XCTestEventKind::CaseStarted {
                test_class: "T".into(),
                method: "testStalls<>&".into(),
            }),
            event(XCTestEventKind::CaseStalled {
                test_class: "T".into(),
                method: "testStalls<>&".into(),
                file: "T.swift".into(),
                line: 9,
            }),
        ];
        let xml = junit_xml(&events);
        assert!(xml.contains("name=\"testStalls&lt;&gt;&amp;\""));
        assert!(xml.contains("<error message=\"stalled\">T.swift:9</error>"));
        assert!(xml.contains("errors=\"1\""));
    }

    #[test]
    fn timestamp_is_utc_iso8601() {
        // 1700000000 秒 = 2023-11-14T22:13:20Z。
        let events = vec![event(XCTestEventKind::CaseStarted {
            test_class: "T".into(),
            method: "testA".into(),
        })];
        let xml = junit_xml(&events);
        assert!(xml.contains("timestamp=\"2023-11-14T22:13:20\""));
    }
}
