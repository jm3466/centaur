"""Discord client: proxied channel access through the Centaur API, plus direct token commands."""

import mimetypes
import os
import re
import time
from datetime import datetime, timezone
from typing import Any
from urllib.parse import quote

import httpx

from centaur_sdk import secret

BASE_URL = "https://discord.com/api/v10"
INVITE_RE = re.compile(r"(?:https?://)?(?:discord(?:\.gg|\.com/invite)/)?([A-Za-z0-9-]+)")
CHANNEL_ID_RE = re.compile(r"\d{17,20}")

# Discord asks automated clients to identify themselves with a descriptive
# User-Agent (https://discord.com/developers/docs/reference#user-agent). When
# this client runs under a *bot* token (Authorization: ``Bot <token>``), sending
# a browser User-Agent trips Discord's edge anti-automation: guild/channel reads
# come back ``403`` with body ``internal network error`` while ``/users/@me``
# still succeeds. A proper bot UA keeps reads working under a bot token (and is
# harmless under a user token).
USER_AGENT = "DiscordBot (https://github.com/paradigmxyz/centaur, 0.1.0)"

# Discord thread channel types
# (https://discord.com/developers/docs/resources/channel#channel-object-channel-types).
THREAD_TYPES = {10: "announcement_thread", 11: "public_thread", 12: "private_thread"}
PUBLIC_THREAD_TYPE = 11
PRIVATE_THREAD_TYPE = 12


class DiscordClient:
    """High-level Discord client.

    Proxied methods (``*_proxy``) call the Centaur API, which holds the bot
    token and checks the conversation's Discord channel permissions. Direct
    methods call Discord with the ``DISCORD_BOT_TOKEN`` credential.
    """

    def __init__(self, token: str | None = None, timeout: float = 30.0):
        self._token = token
        self.timeout = timeout

    def _get_token(self) -> str:
        token = self._token or secret("DISCORD_BOT_TOKEN", "")
        if not token:
            raise RuntimeError("DISCORD_BOT_TOKEN not set.")
        return token

    def _request(self, method: str, endpoint: str, **kwargs) -> dict[str, Any] | list[Any]:
        headers = {
            "Authorization": self._get_token(),
            "Content-Type": "application/json",
            "User-Agent": USER_AGENT,
        }
        with httpx.Client(timeout=self.timeout) as client:
            response = client.request(method, f"{BASE_URL}{endpoint}", headers=headers, **kwargs)
            if response.status_code == 429:
                try:
                    retry_after = float(response.json().get("retry_after", 0))
                except Exception:
                    retry_after = 0
                if 0 < retry_after <= self.timeout:
                    time.sleep(retry_after)
                    response = client.request(method, f"{BASE_URL}{endpoint}", headers=headers, **kwargs)
        if response.status_code == 204:
            return {}
        if response.status_code >= 400:
            try:
                message = response.json().get("message", response.text)
            except Exception:
                message = response.text
            raise RuntimeError(f"Discord API error ({response.status_code}): {message}")
        return response.json()

    def _centaur_api_url(self) -> str:
        """Return the Centaur API base URL available inside agent sandboxes."""
        return secret("CENTAUR_API_URL", "http://api:8000").rstrip("/")

    def _centaur_api_headers(self) -> dict[str, str]:
        """Return headers for API-server calls.

        In sandboxes, iron-proxy injects the principal-scoped Authorization
        header for the API host. A local bearer can be supplied for tests or
        manual CLI use.
        """
        headers = {"Accept": "application/json"}
        bearer = secret("CENTAUR_API_BEARER_TOKEN", "").strip()
        if bearer:
            headers["Authorization"] = f"Bearer {bearer}"
        return headers

    def _centaur_api_request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        **kwargs,
    ) -> dict[str, Any]:
        """Call a Centaur API JSON endpoint."""
        try:
            with httpx.Client(timeout=self.timeout) as client:
                response = client.request(
                    method,
                    f"{self._centaur_api_url()}{path}",
                    headers={**self._centaur_api_headers(), **(headers or {})},
                    **kwargs,
                )
        except httpx.HTTPError as exc:
            raise RuntimeError(f"Centaur API request failed on {path}: {exc}") from exc
        if response.status_code >= 400:
            raise RuntimeError(_centaur_api_error(path, response))
        return response.json()

    def _channel_path(self, channel_id: str) -> str:
        """Return the Centaur API path of an explicit Discord channel or thread ID."""
        normalized = str(channel_id).strip().lstrip("#")
        if not CHANNEL_ID_RE.fullmatch(normalized):
            raise ValueError(
                "channel must be a Discord channel or thread ID like 123456789012345678; "
                "`discord channels` lists the channels this conversation may use"
            )
        return f"/api/discord/channels/{normalized}"

    def list_channels_proxy(self) -> list[dict[str, Any]]:
        """List the channels this conversation may use, with their permissions."""
        return list(self._centaur_api_request("GET", "/api/discord/channels").get("channels", []))

    def get_messages_proxy(self, channel_id: str, limit: int = 50) -> list[dict[str, Any]]:
        """Get recent messages from a channel or thread."""
        page = self._centaur_api_request(
            "GET",
            f"{self._channel_path(channel_id)}/messages",
            params={"limit": min(limit, 100)},
        )
        return list(reversed([self._format_message(msg) for msg in page.get("messages", [])]))

    def search_messages_proxy(self, query: str, channel_id: str, limit: int = 50) -> list[dict[str, Any]]:
        """Search recent messages in one channel or thread."""
        path = f"{self._channel_path(channel_id)}/messages"
        rows = []
        before = None
        while len(rows) < limit:
            params: dict[str, Any] = {"limit": 100}
            if before:
                params["before"] = before
            messages = self._centaur_api_request("GET", path, params=params).get("messages", [])
            if not messages:
                break
            for msg in messages:
                if query.lower() in (msg.get("content") or "").lower():
                    rows.append(self._format_message(msg))
                    if len(rows) >= limit:
                        break
            before = messages[-1].get("id")
        return list(reversed(rows))

    def get_context_proxy(
        self,
        channel_id: str,
        message_id: str,
        before: int = 10,
        after: int = 10,
    ) -> list[dict[str, Any]]:
        """Get messages around a specific message."""
        path = self._channel_path(channel_id)
        target = self._centaur_api_request(
            "GET", f"{path}/messages/{quote(str(message_id), safe='')}"
        ).get("message", {})
        before_msgs = self._centaur_api_request(
            "GET",
            f"{path}/messages",
            params={"limit": min(before, 100), "before": message_id},
        ).get("messages", [])
        after_msgs = self._centaur_api_request(
            "GET",
            f"{path}/messages",
            params={"limit": min(after, 100), "after": message_id},
        ).get("messages", [])
        messages = [*reversed(after_msgs), target, *before_msgs]
        return [self._format_message(msg) for msg in messages]

    def post_message_proxy(
        self,
        channel_id: str,
        content: str,
        reply_to_message_id: str | None = None,
        suppress_embeds: bool = False,
    ) -> dict[str, Any]:
        """Post a message to a channel or thread.

        Set ``suppress_embeds`` to stop Discord unfurling link previews — useful
        for status/alert posts whose URLs should stay compact.
        """
        response = self._centaur_api_request(
            "POST",
            f"{self._channel_path(channel_id)}/messages",
            json={
                "content": content,
                "reply_to": reply_to_message_id,
                "suppress_embeds": suppress_embeds,
            },
        )
        return self._format_message(response.get("message", {}))

    def upload_file_proxy(
        self,
        channel_id: str,
        file_path: str,
        content: str = "",
        reply_to_message_id: str | None = None,
    ) -> dict[str, Any]:
        """Upload a local file to a channel or thread, with optional message text."""
        if not os.path.isfile(file_path):
            raise FileNotFoundError(f"File not found: {file_path}")
        path = f"{self._channel_path(channel_id)}/files"
        params = {
            "filename": os.path.basename(file_path),
            "content": content or None,
            "reply_to": reply_to_message_id,
            "content_type": mimetypes.guess_type(file_path)[0],
        }
        with open(file_path, "rb") as handle:
            response = self._centaur_api_request(
                "POST",
                path,
                params={key: value for key, value in params.items() if value is not None},
                content=handle,
                headers={"Content-Type": "application/octet-stream"},
            )
        return self._format_message(response.get("message", {}))

    def download_message_attachments_proxy(
        self,
        channel_id: str,
        message_id: str,
        output_dir: str = ".",
        attachment_id: str | None = None,
    ) -> list[dict[str, Any]]:
        """Download one attachment, or every attachment, of a message into output_dir."""
        path = f"{self._channel_path(channel_id)}/messages/{quote(str(message_id), safe='')}"
        if attachment_id:
            attachments = [{"id": attachment_id, "filename": attachment_id}]
        else:
            message = self._centaur_api_request("GET", path).get("message", {})
            attachments = message.get("attachments") or []
        os.makedirs(output_dir, exist_ok=True)
        saved = []
        for attachment in attachments:
            attachment_path = f"{path}/attachments/{quote(str(attachment['id']), safe='')}"
            saved.append(
                self._download_proxy(
                    attachment_path,
                    output_dir,
                    attachment.get("filename") or "attachment",
                )
            )
        return saved

    def _download_proxy(self, path: str, output_dir: str, default_filename: str) -> dict[str, Any]:
        """Stream a Centaur API file download into output_dir."""
        try:
            with (
                httpx.Client(timeout=self.timeout) as client,
                client.stream(
                    "GET",
                    f"{self._centaur_api_url()}{path}",
                    headers=self._centaur_api_headers(),
                ) as response,
            ):
                if response.status_code >= 400:
                    response.read()
                    raise RuntimeError(_centaur_api_error(path, response))
                filename = _content_disposition_filename(response.headers.get("content-disposition"))
                # Never let a server-supplied filename escape output_dir.
                name = os.path.basename(filename or default_filename) or "attachment"
                dest = os.path.join(output_dir, name)
                size = 0
                try:
                    with open(dest, "wb") as handle:
                        for chunk in response.iter_bytes():
                            size += len(chunk)
                            handle.write(chunk)
                except BaseException:
                    if os.path.exists(dest):
                        os.unlink(dest)
                    raise
        except httpx.HTTPError as exc:
            raise RuntimeError(f"Centaur API request failed on {path}: {exc}") from exc
        return {"filename": name, "path": dest, "size": size}

    def get_me(self) -> dict[str, Any]:
        """Get the current Discord user."""
        data = dict(self._request("GET", "/users/@me"))
        return {
            "id": data.get("id"),
            "username": data.get("username"),
            "discriminator": data.get("discriminator"),
            "global_name": data.get("global_name"),
        }

    def join_server(self, invite: str) -> dict[str, Any]:
        """Join a server using an invite code or invite URL."""
        match = INVITE_RE.fullmatch(invite.strip())
        if not match:
            raise ValueError("Provide a Discord invite code or URL.")
        return dict(self._request("POST", f"/invites/{match.group(1)}", json={}))

    def list_servers(self, query: str = "", limit: int = 100) -> list[dict[str, Any]]:
        """List joined servers/guilds."""
        rows = []
        for guild in self._request("GET", "/users/@me/guilds"):
            if query and query.lower() not in guild.get("name", "").lower():
                continue
            rows.append(
                {
                    "id": str(guild.get("id", "")),
                    "name": guild.get("name", ""),
                    "member_count": guild.get("approximate_member_count"),
                }
            )
            if len(rows) >= limit:
                break
        return rows

    def list_channels(self, guild: str, query: str = "") -> list[dict[str, Any]]:
        """List text channels in a server by name or ID."""
        resolved = self._find_guild(guild)
        rows = []
        needle = query.lower().lstrip("#")
        for channel in self._guild_channels(resolved["id"]):
            if channel.get("type") != 0:
                continue
            if needle and needle not in channel.get("name", "").lower():
                continue
            rows.append(
                {
                    "id": str(channel.get("id", "")),
                    "name": channel.get("name", ""),
                    "guild_id": str(resolved["id"]),
                    "guild_name": resolved["name"],
                }
            )
        return rows

    def search_messages(self, query: str, channel: str, limit: int = 50) -> list[dict[str, Any]]:
        """Search recent messages in one channel by name or ID."""
        resolved = self._find_channel(channel)
        rows = []
        before = None
        while len(rows) < limit:
            params: dict[str, Any] = {"limit": 100}
            if before:
                params["before"] = before
            messages = self._request("GET", f"/channels/{resolved['id']}/messages", params=params)
            if not messages:
                break
            for msg in messages:
                if query.lower() in (msg.get("content") or "").lower():
                    rows.append(self._format_message(msg, resolved.get("name")))
                    if len(rows) >= limit:
                        break
            before = messages[-1].get("id")
        return list(reversed(rows))

    def search_all(self, guild: str, query: str, limit: int = 50) -> list[dict[str, Any]]:
        """Search messages across a server by name or ID."""
        resolved = self._find_guild(guild)
        channels = [c for c in self._guild_channels(resolved["id"]) if c.get("type") == 0]
        rows = []
        for channel in channels:
            try:
                rows.extend(self.search_messages(query, channel["id"], limit=limit - len(rows)))
            except RuntimeError:
                continue
            if len(rows) >= limit:
                break
        return rows

    def create_thread(
        self,
        channel: str,
        name: str,
        from_message_id: str | None = None,
        content: str | None = None,
        private: bool = False,
    ) -> dict[str, Any]:
        """Create a thread in a channel by name or ID.

        Pass from_message_id to branch a public thread off an existing message.
        Otherwise a standalone thread is created (public by default, or private
        when private is set), and content, if given, is posted as its first message.
        """
        resolved = self._find_channel(channel)
        if from_message_id:
            thread = self._request(
                "POST",
                f"/channels/{resolved['id']}/messages/{from_message_id}/threads",
                json={"name": name},
            )
            return self._format_thread(thread)
        thread_type = PRIVATE_THREAD_TYPE if private else PUBLIC_THREAD_TYPE
        thread = self._request(
            "POST",
            f"/channels/{resolved['id']}/threads",
            json={"name": name, "type": thread_type},
        )
        if content:
            self._request("POST", f"/channels/{thread['id']}/messages", json={"content": content})
        return self._format_thread(thread)

    def _guild_channels(self, guild_id: str) -> list[dict[str, Any]]:
        return list(self._request("GET", f"/guilds/{guild_id}/channels"))

    def _find_guild(self, guild_str: str) -> dict[str, Any]:
        guilds = list(self._request("GET", "/users/@me/guilds"))
        if guild_str.isdigit():
            for guild in guilds:
                if guild.get("id") == guild_str:
                    return {"id": str(guild["id"]), "name": guild.get("name", "")}
        for guild in guilds:
            if guild.get("name", "").lower() == guild_str.lower():
                return {"id": str(guild["id"]), "name": guild.get("name", "")}
        for guild in guilds:
            if guild_str.lower() in guild.get("name", "").lower():
                return {"id": str(guild["id"]), "name": guild.get("name", "")}
        raise RuntimeError(f"Guild not found: {guild_str}")

    def _find_channel(self, channel_str: str) -> dict[str, Any]:
        if channel_str.isdigit():
            channel = dict(self._request("GET", f"/channels/{channel_str}"))
            return {
                "id": str(channel["id"]),
                "name": channel.get("name"),
                "guild_id": str(channel.get("guild_id", "")),
                "guild_name": None,
            }
        needle = channel_str.lstrip("#").lower()
        partial = None
        for guild in self._request("GET", "/users/@me/guilds"):
            for channel in self._guild_channels(guild["id"]):
                if channel.get("type") != 0:
                    continue
                name = channel.get("name", "")
                if name.lower() == needle:
                    return {
                        "id": str(channel["id"]),
                        "name": name,
                        "guild_id": str(guild["id"]),
                        "guild_name": guild.get("name"),
                    }
                if partial is None and needle in name.lower():
                    partial = {
                        "id": str(channel["id"]),
                        "name": name,
                        "guild_id": str(guild["id"]),
                        "guild_name": guild.get("name"),
                    }
        if partial:
            return partial
        raise RuntimeError(f"Channel not found: {channel_str}")

    def _format_message(self, msg: dict[str, Any], channel_name: str | None = None) -> dict[str, Any]:
        author = msg.get("author") or {}
        return {
            "id": str(msg.get("id", "")),
            "channel_id": str(msg.get("channel_id", "")),
            "channel_name": channel_name,
            "author": author.get("global_name") or author.get("username") or "",
            "author_id": str(author.get("id", "")),
            "timestamp": _format_timestamp(msg),
            "content": msg.get("content") or "",
            "reply_to": ((msg.get("message_reference") or {}).get("message_id")),
            "attachments": [
                {
                    "id": str(attachment.get("id", "")),
                    "filename": attachment.get("filename"),
                    "url": attachment.get("url"),
                    "size": attachment.get("size"),
                    "content_type": attachment.get("content_type"),
                }
                for attachment in (msg.get("attachments") or [])
            ],
        }

    def _format_thread(self, thread: dict[str, Any]) -> dict[str, Any]:
        metadata = thread.get("thread_metadata") or {}
        guild_id = str(thread.get("guild_id", ""))
        thread_id = str(thread.get("id", ""))
        return {
            "id": thread_id,
            "name": thread.get("name"),
            "parent_id": str(thread.get("parent_id", "")),
            "guild_id": guild_id,
            "owner_id": str(thread.get("owner_id", "")),
            "type": THREAD_TYPES.get(thread.get("type")),
            "archived": metadata.get("archived", False),
            "url": f"https://discord.com/channels/{guild_id}/{thread_id}",
        }


def _client() -> DiscordClient:
    return DiscordClient()


def _centaur_api_error(path: str, response: httpx.Response) -> str:
    """Describe a failed Centaur API response, including any Retry-After."""
    try:
        body = response.json()
        detail = body.get("error") or body.get("message") or response.text
    except ValueError:
        detail = response.text
    message = f"Centaur API error {response.status_code} on {path}: {detail}"
    retry_after = response.headers.get("retry-after")
    if retry_after:
        message += f" (retry after {retry_after}s)"
    return message


def _content_disposition_filename(value: str | None) -> str | None:
    """Extract a simple filename value from Content-Disposition."""
    if not value:
        return None
    match = re.search(r'filename="([^"]+)"', value)
    if match:
        return match.group(1)
    return None


def _format_timestamp(msg: dict[str, Any]) -> str:
    timestamp = msg.get("timestamp")
    if timestamp:
        return str(timestamp)
    snowflake = msg.get("id")
    if not snowflake:
        return ""
    created_ms = (int(snowflake) >> 22) + 1420070400000
    return datetime.fromtimestamp(created_ms / 1000, tz=timezone.utc).isoformat()
