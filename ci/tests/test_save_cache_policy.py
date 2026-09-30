"""Decision table for the runner-aware ``save-cache`` input (#1783)."""

import pytest

from ci import save_cache_policy as policy

GITHUB_ENV = {"ACTIONS_CACHE_URL": "https://artifactcache.actions.githubusercontent.com/abc/"}


def _save(mode: str, event: str, env: dict) -> bool:
    return policy.decide(policy.parse_mode(mode), event, env)[0]


def test_auto_pull_request_under_act_saves() -> None:
    assert _save("auto", "pull_request", {"ACT": "true"}) is True


def test_auto_pull_request_on_github_skips() -> None:
    assert _save("auto", "pull_request", GITHUB_ENV) is False


def test_auto_push_saves() -> None:
    assert _save("auto", "push", GITHUB_ENV) is True


@pytest.mark.parametrize(
    "url",
    [
        "http://127.0.0.1:34567/",
        "http://localhost:9000/",
        "http://[::1]:9000/",
        "http://10.1.2.3:9000/",
        "http://172.17.0.1:9000/",
        "http://192.168.1.5:9000/",
        "http://169.254.10.1/",
    ],
)
@pytest.mark.parametrize("var", ["ACTIONS_CACHE_URL", "ACTIONS_RESULTS_URL"])
def test_local_cache_endpoint_is_local_runner(var: str, url: str) -> None:
    env = {var: url}
    assert policy.detect_runner(env)[0] == "local"
    assert _save("auto", "pull_request", env) is True


@pytest.mark.parametrize(
    "url",
    ["", "https://results-receiver.actions.githubusercontent.com/", "http://20.1.2.3/", "not a url"],
)
def test_public_or_missing_cache_endpoint_is_github(url: str) -> None:
    assert policy.detect_runner({"ACTIONS_CACHE_URL": url})[0] == "github"


def test_act_must_be_exactly_true() -> None:
    assert policy.detect_runner({"ACT": "false"})[0] == "github"


@pytest.mark.parametrize("event", ["pull_request", "push", "schedule"])
@pytest.mark.parametrize("env", [{"ACT": "true"}, GITHUB_ENV])
def test_explicit_true_and_false_ignore_runner_and_event(event: str, env: dict) -> None:
    assert _save("true", event, env) is True
    assert _save("false", event, env) is False


def test_empty_mode_is_auto_and_aliases_parse() -> None:
    assert policy.parse_mode("") == "auto"
    assert policy.parse_mode(None) == "auto"
    assert policy.parse_mode(" TRUE ") == "true"
    assert policy.parse_mode("off") == "false"
    with pytest.raises(ValueError):
        policy.parse_mode("sometimes")


def test_log_line_names_runner_and_decision() -> None:
    _, line = policy.decide("auto", "pull_request", {"ACT": "true"})
    assert line == "save-policy: runner=local (ACT=true) mode=auto → save"
    _, line = policy.decide("auto", "pull_request", GITHUB_ENV)
    assert line == "save-policy: runner=github event=pull_request mode=auto → skip"


def test_main_prints_resolved_value_and_logs_once(capsys) -> None:
    env = {"INPUT_SAVE_CACHE": "auto", "GITHUB_EVENT_NAME": "pull_request", "ACT": "true"}
    assert policy.main(env) == 0
    out = capsys.readouterr()
    assert out.out.strip() == "true"
    assert out.err.count("save-policy:") == 1


def test_main_rejects_invalid_mode(capsys) -> None:
    assert policy.main({"INPUT_SAVE_CACHE": "bogus"}) == 2
