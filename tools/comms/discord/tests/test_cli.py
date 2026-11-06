import sys
import types

from discord.cli import app
from typer.testing import CliRunner

CHANNEL = "123456789012345678"


def _fake_client(monkeypatch, **methods):
    monkeypatch.setitem(
        sys.modules,
        "discord.client",
        types.SimpleNamespace(DiscordClient=lambda: types.SimpleNamespace(**methods)),
    )


def test_token_commands_carry_the_direct_suffix():
    names = {command.name for command in app.registered_commands}

    assert names == {
        "health",
        "channels",
        "messages",
        "search",
        "context",
        "post",
        "upload",
        "download",
        "me-direct",
        "join-server-direct",
        "servers-direct",
        "channels-direct",
        "search-all-direct",
        "create-thread-direct",
    }


def test_post_uses_the_proxied_client(monkeypatch):
    calls = []

    def post_message_proxy(**kwargs):
        calls.append(kwargs)
        return {"id": "900", "channel_id": kwargs["channel_id"]}

    _fake_client(monkeypatch, post_message_proxy=post_message_proxy)

    result = CliRunner().invoke(app, ["post", CHANNEL, "hi", "--reply-to", "899"])

    assert result.exit_code == 0, result.output
    assert calls == [
        {
            "channel_id": CHANNEL,
            "content": "hi",
            "reply_to_message_id": "899",
            "suppress_embeds": False,
        }
    ]
    assert "Sent" in result.output


def test_download_passes_one_attachment_id(monkeypatch, tmp_path):
    calls = []

    def download_message_attachments_proxy(**kwargs):
        calls.append(kwargs)
        return [{"filename": "a.txt", "path": str(tmp_path / "a.txt"), "size": 1}]

    _fake_client(monkeypatch, download_message_attachments_proxy=download_message_attachments_proxy)

    result = CliRunner().invoke(
        app, ["download", CHANNEL, "700", "--attachment-id", "701", "-o", str(tmp_path)]
    )

    assert result.exit_code == 0, result.output
    assert calls == [
        {
            "channel_id": CHANNEL,
            "message_id": "700",
            "output_dir": str(tmp_path),
            "attachment_id": "701",
        }
    ]
