from datetime import datetime, timezone

from ci import host_diag


def test_new_run_id_formats_timestamp_and_truncates_entropy():
    now_utc = datetime(2026, 7, 23, 4, 5, 6, tzinfo=timezone.utc)
    entropy = "abcdef0123456789"

    assert host_diag.new_run_id(now_utc, entropy) == "hd-20260723T040506Z-abcdef01"


def test_new_run_id_short_entropy_is_not_padded():
    now_utc = datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone.utc)

    assert host_diag.new_run_id(now_utc, "ab") == "hd-20260102T030405Z-ab"


def test_format_stream_line_zero_nanoseconds():
    assert host_diag.format_stream_line(0, "hello") == "[+00000.000000s] hello"


def test_format_stream_line_sub_second_nanoseconds():
    assert (
        host_diag.format_stream_line(123_456_000, "tick")
        == "[+00000.123456s] tick"
    )


def test_format_stream_line_large_value():
    assert (
        host_diag.format_stream_line(12_345_678_900_000, "boom")
        == "[+12345.678900s] boom"
    )


def test_parse_journal_lines_mix_of_valid_and_malformed():
    lines = [
        '{"outcome": "hit", "ts": 1}',
        "[1, 2, 3]",
        "not json at all",
        "",
        "   ",
        '{"outcome": "miss", "ts": 2}',
    ]

    records, malformed_count = host_diag.parse_journal_lines(lines)

    assert records == [
        {"outcome": "hit", "ts": 1},
        {"outcome": "miss", "ts": 2},
    ]
    assert malformed_count == 2


def test_parse_journal_lines_all_blank_yields_no_records_and_no_malformed():
    records, malformed_count = host_diag.parse_journal_lines(["", "   ", "\t"])

    assert records == []
    assert malformed_count == 0


def test_summarize_journal_single_session_hit_rate_and_aggregate():
    records = [
        {"ts": 1, "session_id": "s1", "outcome": "hit"},
        {"ts": 2, "session_id": "s1", "outcome": "hit"},
        {"ts": 3, "session_id": "s1", "outcome": "hit"},
        {"ts": 4, "session_id": "s1", "outcome": "link_hit"},
        {"ts": 5, "session_id": "s1", "outcome": "miss", "miss_reason": "no_key"},
        {"ts": 6, "session_id": "s1", "outcome": "miss", "miss_reason": "no_key"},
    ]

    summary = host_diag.summarize_journal(records, window_start_ns=0, window_end_ns=10)

    assert summary["total"] == 6
    assert summary["in_window"] == 6
    assert summary["overlapping"] is False
    assert summary["aggregate_rejected_reason"] is None

    assert len(summary["sessions"]) == 1
    session = summary["sessions"][0]
    assert session["session_id"] == "s1"
    assert session["first_ts"] == 1
    assert session["last_ts"] == 6
    assert session["records"] == 6
    assert session["outcomes"] == {"hit": 3, "link_hit": 1, "miss": 2}
    assert session["miss_reasons"] == {"no_key": 2}
    assert session["hit_rate"] == 4 / 6

    aggregate = summary["aggregate"]
    assert aggregate is not None
    assert "session_id" not in aggregate
    assert aggregate["first_ts"] == 1
    assert aggregate["last_ts"] == 6
    assert aggregate["records"] == 6
    assert aggregate["outcomes"] == {"hit": 3, "link_hit": 1, "miss": 2}
    assert aggregate["miss_reasons"] == {"no_key": 2}
    assert aggregate["hit_rate"] == 4 / 6


def test_summarize_journal_overlapping_sessions_rejects_aggregate():
    records = [
        {"ts": 1, "session_id": "a", "outcome": "hit"},
        {"ts": 5, "session_id": "a", "outcome": "hit"},
        {"ts": 3, "session_id": "b", "outcome": "miss"},
        {"ts": 8, "session_id": "b", "outcome": "miss"},
    ]

    summary = host_diag.summarize_journal(records, window_start_ns=0, window_end_ns=10)

    assert summary["overlapping"] is True
    assert summary["aggregate"] is None
    assert summary["aggregate_rejected_reason"] == "overlapping-sessions"

    session_ids = [s["session_id"] for s in summary["sessions"]]
    assert session_ids == ["a", "b"]
    first_ts_values = [s["first_ts"] for s in summary["sessions"]]
    assert first_ts_values == sorted(first_ts_values)


def test_summarize_journal_non_overlapping_sessions_aggregate_covers_both():
    records = [
        {"ts": 1, "session_id": "a", "outcome": "hit"},
        {"ts": 2, "session_id": "a", "outcome": "hit"},
        {"ts": 10, "session_id": "b", "outcome": "miss"},
        {"ts": 11, "session_id": "b", "outcome": "miss"},
    ]

    summary = host_diag.summarize_journal(records, window_start_ns=0, window_end_ns=20)

    assert summary["overlapping"] is False
    assert summary["aggregate_rejected_reason"] is None

    aggregate = summary["aggregate"]
    assert aggregate is not None
    assert aggregate["records"] == 4
    assert aggregate["outcomes"] == {"hit": 2, "miss": 2}
    assert aggregate["first_ts"] == 1
    assert aggregate["last_ts"] == 11

    session_ids = [s["session_id"] for s in summary["sessions"]]
    assert session_ids == ["a", "b"]


def test_summarize_journal_window_filtering_and_boundaries():
    records = [
        {"ts": 5, "session_id": "s", "outcome": "hit"},  # == start, included
        {"ts": 10, "session_id": "s", "outcome": "hit"},  # == end, included
        {"ts": 4, "session_id": "s", "outcome": "hit"},  # before window, excluded
        {"ts": 11, "session_id": "s", "outcome": "hit"},  # after window, excluded
        {"session_id": "s", "outcome": "miss"},  # missing ts, excluded from window
        {"ts": "not-a-number", "session_id": "s", "outcome": "miss"},  # non-numeric
    ]

    summary = host_diag.summarize_journal(records, window_start_ns=5, window_end_ns=10)

    assert summary["total"] == 6
    assert summary["in_window"] == 2


def test_summarize_journal_empty_in_window_yields_no_aggregate():
    records = [
        {"ts": 1, "session_id": "s", "outcome": "hit"},
    ]

    summary = host_diag.summarize_journal(records, window_start_ns=100, window_end_ns=200)

    assert summary["total"] == 1
    assert summary["in_window"] == 0
    assert summary["aggregate"] is None


def test_summarize_journal_hit_rate_none_without_hit_or_miss_outcomes():
    records = [
        {"ts": 1, "session_id": "s", "outcome": "error"},
        {"ts": 2, "session_id": "s", "outcome": "error"},
    ]

    summary = host_diag.summarize_journal(records, window_start_ns=0, window_end_ns=10)

    session = summary["sessions"][0]
    assert session["hit_rate"] is None
    assert summary["aggregate"]["hit_rate"] is None


def test_summarize_journal_none_session_id_is_a_valid_group():
    records = [
        {"ts": 1, "outcome": "hit"},
        {"ts": 2, "outcome": "hit"},
    ]

    summary = host_diag.summarize_journal(records, window_start_ns=0, window_end_ns=10)

    assert len(summary["sessions"]) == 1
    assert summary["sessions"][0]["session_id"] is None


def test_scan_auto_gc_log_counts_starts_and_terminals():
    lines = [
        "stage=start id=1",
        "stage=start id=2",
        "stage=start id=3",
        "stage=done id=1",
        "some unrelated line",
    ]

    result = host_diag.scan_auto_gc_log(lines)

    assert result == {"starts": 3, "terminals": 1, "unterminated": 2}


def test_scan_auto_gc_log_terminals_matches_detected_and_warning():
    lines = [
        "stage=start id=1",
        "detected leftover artifact",
        "warning: disk low",
    ]

    result = host_diag.scan_auto_gc_log(lines)

    assert result == {"starts": 1, "terminals": 2, "unterminated": 0}


def test_scan_auto_gc_log_unterminated_never_negative():
    lines = [
        "stage=start id=1",
        "stage=done id=1",
        "stage=done id=2",
        "detected extra",
    ]

    result = host_diag.scan_auto_gc_log(lines)

    assert result["starts"] == 1
    assert result["terminals"] == 3
    assert result["unterminated"] == 0


def test_build_report_passes_through_all_fields():
    environment = {"os": "windows", "rustc": "1.94.1"}
    gates = [{"name": "perf", "passed": True}]
    journal = {"total": 1}
    auto_gc = {"starts": 1, "terminals": 1, "unterminated": 0}

    report = host_diag.build_report("hd-run-id", environment, gates, journal, auto_gc)

    assert report == {
        "schema_version": 1,
        "run_id": "hd-run-id",
        "environment": environment,
        "gates": gates,
        "journal": journal,
        "auto_gc": auto_gc,
    }


def test_build_report_allows_none_journal_and_auto_gc():
    report = host_diag.build_report("hd-run-id", {}, [], None, None)

    assert report["journal"] is None
    assert report["auto_gc"] is None
