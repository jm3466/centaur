import sys
from pathlib import Path

import httpx
import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from client import DiscordClient

CHANNEL = "123456789012345678"
THREAD = "223456789012345678"


class _FakeResponse:
    """Minimal stand-in for an ``httpx`` response, plain or streamed."""

    def __init__(self, status_code=200, json_body=None, headers=None, chunks=()):
        self.status_code = status_code
        self._json = json_body
        self.headers = httpx.Headers(headers or {})
        self._chunks = chunks
        self.text = "" if json_body is None else str(json_body)

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def json(self):
        if self._json is None:
            raise ValueError("no JSON body")
        return self._json

    def read(self):
        return b""

    def iter_bytes(self):
        yield from self._chunks


def _fake_centaur_api(monkeypatch, respond):
    """Route every httpx call to ``respond(method, url, params)`` and record it."""
    calls = []

    class FakeClient:
        def __init__(self, *args, **kwargs):
            pass

        def __enter__(self):
            return self

        def __exit__(self, *args):
            return False

        def request(self, method, url, headers=None, params=None, **kwargs):
            content = kwargs.get("content")
            if hasattr(content, "read"):
                kwargs["content"] = content.read()
            calls.append({"method": method, "url": url, "headers": headers, "params": params, **kwargs})
            return respond(method, url, params)

        def stream(self, method, url, headers=None):
            calls.append({"method": method, "url": url, "headers": headers, "stream": True})
            return respond(method, url, None)

    def no_bot_token(key, default=None):
        assert key != "DISCORD_BOT_TOKEN", "proxied commands must not use the bot token"
        return {"CENTAUR_API_URL": "http://api.test/", "CENTAUR_API_BEARER_TOKEN": ""}.get(key, default)

    monkeypatch.setattr("client.httpx.Client", FakeClient)
    monkeypatch.setattr("client.secret", no_bot_token)
    return calls


def test_join_server_posts_invite_code(monkeypatch):
    client = DiscordClient(token="unused")

    def fake_request(method, endpoint, **kwargs):
        assert method == "POST"
        assert endpoint == "/invites/abc123"
        return {"code": "abc123", "guild": {"name": "Test"}}

    monkeypatch.setattr(client, "_request", fake_request)

    assert client.join_server("https://discord.gg/abc123")["guild"]["name"] == "Test"


def test_list_servers_uses_rest(monkeypatch):
    client = DiscordClient(token="unused")

    def fake_request(method, endpoint, **kwargs):
        assert method == "GET"
        assert endpoint == "/users/@me/guilds"
        return [
            {"id": "1", "name": "General", "approximate_member_count": 10},
            {"id": "2", "name": "Eth R&D", "approximate_member_count": 20},
        ]

    monkeypatch.setattr(client, "_request", fake_request)

    assert client.list_servers("eth") == [{"id": "2", "name": "Eth R&D", "member_count": 20}]


def test_find_guild_exact_then_partial_name(monkeypatch):
    client = DiscordClient(token="unused")

    def fake_request(method, endpoint, **kwargs):
        assert method == "GET"
        assert endpoint == "/users/@me/guilds"
        return [
            {"id": "1", "name": "General"},
            {"id": "2", "name": "Eth R&D"},
        ]

    monkeypatch.setattr(client, "_request", fake_request)

    assert client._find_guild("Eth R&D")["id"] == "2"
    assert client._find_guild("eth")["id"] == "2"


def test_find_channel_supports_hash_prefix_and_partial_name(monkeypatch):
    client = DiscordClient(token="unused")

    def fake_request(method, endpoint, **kwargs):
        assert method == "GET"
        if endpoint == "/users/@me/guilds":
            return [{"id": "1", "name": "General"}]
        if endpoint == "/guilds/1/channels":
            return [{"id": "11", "name": "announcements", "type": 0}]
        raise AssertionError(endpoint)

    monkeypatch.setattr(client, "_request", fake_request)

    assert client._find_channel("#announcements")["id"] == "11"
    assert client._find_channel("announce")["id"] == "11"


def test_find_channel_by_id_uses_channel_endpoint(monkeypatch):
    client = DiscordClient(token="unused")

    def fake_request(method, endpoint, **kwargs):
        assert method == "GET"
        assert endpoint == "/channels/11"
        return {"id": "11", "name": "announcements", "guild_id": "1"}

    monkeypatch.setattr(client, "_request", fake_request)

    assert client._find_channel("11") == {
        "id": "11",
        "name": "announcements",
        "guild_id": "1",
        "guild_name": None,
    }


def test_format_message_surfaces_attachments():
    client = DiscordClient(token="unused")
    msg = {
        "id": "99",
        "channel_id": "11",
        "author": {"id": "7", "global_name": "Ada"},
        "timestamp": "2026-01-01T00:00:00",
        "content": "see file",
        "attachments": [
            {
                "id": "123",
                "filename": "report.pdf",
                "url": "https://cdn.discordapp.com/attachments/11/123/report.pdf",
                "size": 2048,
                "content_type": "application/pdf",
            }
        ],
    }

    formatted = client._format_message(msg)
    assert formatted["attachments"] == [
        {
            "id": "123",
            "filename": "report.pdf",
            "url": "https://cdn.discordapp.com/attachments/11/123/report.pdf",
            "size": 2048,
            "content_type": "application/pdf",
        }
    ]


def test_format_message_handles_no_attachments():
    client = DiscordClient(token="unused")
    msg = {
        "id": "99",
        "channel_id": "11",
        "author": {"id": "7", "global_name": "Ada"},
        "timestamp": "2026-01-01T00:00:00",
        "content": "hi",
    }

    assert client._format_message(msg)["attachments"] == []


def test_proxied_messages_call_the_centaur_api_without_a_bot_token(monkeypatch):
    calls = _fake_centaur_api(
        monkeypatch,
        lambda method, url, params: _FakeResponse(
            json_body={
                "ok": True,
                "channel_id": THREAD,
                "messages": [
                    {"id": "3", "channel_id": THREAD, "content": "newest"},
                    {"id": "2", "channel_id": THREAD, "content": "older"},
                ],
            }
        ),
    )

    messages = DiscordClient().get_messages_proxy(THREAD, limit=500)

    assert [message["content"] for message in messages] == ["older", "newest"]
    assert calls[0]["method"] == "GET"
    assert calls[0]["url"] == f"http://api.test/api/discord/channels/{THREAD}/messages"
    assert calls[0]["params"] == {"limit": 100}
    assert "Authorization" not in calls[0]["headers"]


def test_proxied_commands_require_an_explicit_channel_id():
    with pytest.raises(ValueError, match="channel or thread ID"):
        DiscordClient().get_messages_proxy("general")


def test_get_context_proxy_reads_the_target_and_its_neighbours(monkeypatch):
    def respond(method, url, params):
        if url.endswith("/messages/500"):
            return _FakeResponse(json_body={"message": {"id": "500", "content": "target"}})
        if params == {"limit": 2, "before": "500"}:
            return _FakeResponse(json_body={"messages": [{"id": "499"}, {"id": "498"}]})
        if params == {"limit": 1, "after": "500"}:
            return _FakeResponse(json_body={"messages": [{"id": "501"}]})
        raise AssertionError((url, params))

    _fake_centaur_api(monkeypatch, respond)

    messages = DiscordClient().get_context_proxy(CHANNEL, "500", before=2, after=1)

    assert [message["id"] for message in messages] == ["501", "500", "499", "498"]


def test_post_message_proxy_sends_the_message_fields(monkeypatch):
    calls = _fake_centaur_api(
        monkeypatch,
        lambda method, url, params: _FakeResponse(
            json_body={"ok": True, "message": {"id": "900", "channel_id": THREAD, "content": "hi"}}
        ),
    )

    result = DiscordClient().post_message_proxy(
        THREAD, "hi", reply_to_message_id="899", suppress_embeds=True
    )

    assert result["id"] == "900"
    assert calls[0]["method"] == "POST"
    assert calls[0]["url"] == f"http://api.test/api/discord/channels/{THREAD}/messages"
    assert calls[0]["json"] == {"content": "hi", "reply_to": "899", "suppress_embeds": True}


def test_upload_file_proxy_posts_the_file_bytes(monkeypatch, tmp_path):
    path = tmp_path / "report.pdf"
    path.write_bytes(b"%PDF-1.")
    calls = _fake_centaur_api(
        monkeypatch,
        lambda method, url, params: _FakeResponse(json_body={"message": {"id": "901"}}),
    )

    result = DiscordClient().upload_file_proxy(CHANNEL, str(path), content="see attached")

    assert result["id"] == "901"
    assert calls[0]["url"] == f"http://api.test/api/discord/channels/{CHANNEL}/files"
    assert calls[0]["params"] == {
        "filename": "report.pdf",
        "content": "see attached",
        "content_type": "application/pdf",
    }
    assert calls[0]["content"] == b"%PDF-1."
    assert calls[0]["headers"]["Content-Type"] == "application/octet-stream"


def test_upload_file_proxy_rejects_missing_path(tmp_path):
    with pytest.raises(FileNotFoundError, match="nope.txt"):
        DiscordClient().upload_file_proxy(CHANNEL, str(tmp_path / "nope.txt"))


def test_download_proxy_reads_the_message_then_streams_each_attachment(monkeypatch, tmp_path):
    def respond(method, url, params):
        if url.endswith("/messages/700"):
            return _FakeResponse(
                json_body={"message": {"attachments": [{"id": "701", "filename": "a.txt"}]}}
            )
        if url.endswith("/messages/700/attachments/701"):
            return _FakeResponse(
                headers={"content-disposition": 'attachment; filename="../a.txt"'},
                chunks=[b"hello-", b"bytes"],
            )
        raise AssertionError(url)

    calls = _fake_centaur_api(monkeypatch, respond)

    saved = DiscordClient().download_message_attachments_proxy(CHANNEL, "700", output_dir=str(tmp_path))

    assert saved == [{"filename": "a.txt", "path": str(tmp_path / "a.txt"), "size": 11}]
    assert (tmp_path / "a.txt").read_bytes() == b"hello-bytes"
    assert calls[1]["url"] == f"http://api.test/api/discord/channels/{CHANNEL}/messages/700/attachments/701"
    assert calls[1]["stream"] is True


def test_download_proxy_can_fetch_one_attachment_without_reading_history(monkeypatch, tmp_path):
    calls = _fake_centaur_api(
        monkeypatch,
        lambda method, url, params: _FakeResponse(
            headers={"content-disposition": 'attachment; filename="b.png"'}, chunks=[b"png"]
        ),
    )

    saved = DiscordClient().download_message_attachments_proxy(
        CHANNEL, "700", output_dir=str(tmp_path), attachment_id="702"
    )

    assert saved == [{"filename": "b.png", "path": str(tmp_path / "b.png"), "size": 3}]
    assert [call["url"] for call in calls] == [
        f"http://api.test/api/discord/channels/{CHANNEL}/messages/700/attachments/702"
    ]


def test_proxied_errors_report_status_and_retry_after(monkeypatch):
    _fake_centaur_api(
        monkeypatch,
        lambda method, url, params: _FakeResponse(
            status_code=429,
            json_body={"ok": False, "error": "Discord message list is rate limited"},
            headers={"retry-after": "3"},
        ),
    )

    with pytest.raises(RuntimeError) as error:
        DiscordClient().get_messages_proxy(CHANNEL)

    assert str(error.value) == (
        f"Centaur API error 429 on /api/discord/channels/{CHANNEL}/messages: "
        "Discord message list is rate limited (retry after 3s)"
    )


def _thread_response(thread_type=11):
    return {
        "id": "99",
        "name": "design chat",
        "parent_id": "11",
        "guild_id": "1",
        "owner_id": "5",
        "type": thread_type,
        "thread_metadata": {"archived": False},
    }


def test_create_thread_from_message_branches_off_starter(monkeypatch):
    client = DiscordClient(token="unused")
    posts = []

    def fake_request(method, endpoint, **kwargs):
        if method == "POST":
            posts.append((endpoint, kwargs.get("json")))
        if endpoint == "/channels/11":
            return {"id": "11", "name": "general", "guild_id": "1"}
        if endpoint == "/channels/11/messages/123/threads":
            return _thread_response()
        raise AssertionError(f"{method} {endpoint}")

    monkeypatch.setattr(client, "_request", fake_request)

    result = client.create_thread("11", "design chat", from_message_id="123")

    # Exactly one POST: branch the thread off the message, no seed message.
    assert posts == [("/channels/11/messages/123/threads", {"name": "design chat"})]
    assert result == {
        "id": "99",
        "name": "design chat",
        "parent_id": "11",
        "guild_id": "1",
        "owner_id": "5",
        "type": "public_thread",
        "archived": False,
        "url": "https://discord.com/channels/1/99",
    }


def test_create_thread_standalone_posts_initial_content(monkeypatch):
    client = DiscordClient(token="unused")
    posts = []

    def fake_request(method, endpoint, **kwargs):
        if method == "POST":
            posts.append((endpoint, kwargs.get("json")))
        if endpoint == "/channels/11":
            return {"id": "11", "name": "general", "guild_id": "1"}
        if endpoint == "/channels/11/threads":
            return _thread_response(thread_type=12)
        if endpoint == "/channels/99/messages":
            return {"id": "1000"}
        raise AssertionError(f"{method} {endpoint}")

    monkeypatch.setattr(client, "_request", fake_request)

    result = client.create_thread("11", "design chat", content="kickoff", private=True)

    assert posts[0] == ("/channels/11/threads", {"name": "design chat", "type": 12})
    assert posts[1] == ("/channels/99/messages", {"content": "kickoff"})
    assert result["id"] == "99"
    assert result["type"] == "private_thread"
