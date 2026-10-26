//! Read path. Queries only the view tables; never Loro or the network.

use rusqlite::{OptionalExtension, Row, params};

use super::Replica;
use crate::auth::Reason;
use crate::error::{Error, Result};
use crate::ids::{DeviceKey, EventId, Principal, Role, ScopeId, UserKey};
use crate::payload::{self, BlobRef, MessageRole, ScopeKind};
use crate::store::{delivery, replication};
use crate::types::{
    AppDataView, BlobInfo, ConversationFilter, ConversationSummary, Cursor, DeliveryStatus, DeviceInfo, MemberInfo, MessagePage, MessageView, ReactionSummary,
    Replication, SearchHit,
};

const MESSAGE_COLUMNS: &str =
    "id, scope, hlc, author_device, principal, is_agent, role, content, blobs, reply_to, tool_call_id, tool_name, edited, status, reason";

fn role_from(i: i64) -> MessageRole {
    match i {
        1 => MessageRole::Assistant,
        2 => MessageRole::System,
        3 => MessageRole::ToolCall,
        4 => MessageRole::ToolResult,
        _ => MessageRole::User,
    }
}

struct RawMessage {
    id: Vec<u8>,
    scope: Vec<u8>,
    hlc: i64,
    author: Vec<u8>,
    principal: String,
    is_agent: bool,
    role: i64,
    content: String,
    blobs: Vec<u8>,
    reply_to: Option<Vec<u8>>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    edited: bool,
    status: i64,
    reason: Option<i64>,
}

fn raw(r: &Row) -> rusqlite::Result<RawMessage> {
    Ok(RawMessage {
        id: r.get(0)?,
        scope: r.get(1)?,
        hlc: r.get(2)?,
        author: r.get(3)?,
        principal: r.get(4)?,
        is_agent: r.get(5)?,
        role: r.get(6)?,
        content: r.get(7)?,
        blobs: r.get(8)?,
        reply_to: r.get(9)?,
        tool_call_id: r.get(10)?,
        tool_name: r.get(11)?,
        edited: r.get(12)?,
        status: r.get(13)?,
        reason: r.get(14)?,
    })
}

/// SQL conditions (each starting with ` AND `) for a conversation filter, with their values
/// numbered from `?{first}`. `scope_col` names the scope column; `c` must be
/// `conversations_view`.
fn filter_sql(f: &ConversationFilter, scope_col: &str, first: usize) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut sql = String::new();
    let mut values: Vec<Value> = Vec::new();
    let next = |v: Value, values: &mut Vec<Value>| {
        values.push(v);
        format!("?{}", first + values.len() - 1)
    };
    for (k, v) in &f.tags {
        let (pk, pv) = (next(k.clone().into(), &mut values), next(v.clone().into(), &mut values));
        sql.push_str(&format!(" AND EXISTS(SELECT 1 FROM conversation_tags t WHERE t.scope = {scope_col} AND t.key = {pk} AND t.value = {pv})"));
    }
    for k in &f.has_tags {
        let pk = next(k.clone().into(), &mut values);
        sql.push_str(&format!(" AND EXISTS(SELECT 1 FROM conversation_tags t WHERE t.scope = {scope_col} AND t.key = {pk})"));
    }
    if let Some(kind) = f.kind {
        let p = next(Value::Integer(if kind == ScopeKind::Workspace { 1 } else { 0 }), &mut values);
        sql.push_str(&format!(" AND c.kind = {p}"));
    }
    if let Some(r) = f.replication {
        let v = if r == Replication::DeviceLocal { replication::DEVICE_LOCAL } else { replication::SYNCED };
        let p = next(Value::Integer(v), &mut values);
        sql.push_str(&format!(" AND COALESCE((SELECT replication FROM scopes s WHERE s.scope = {scope_col}), 0) = {p}"));
    }
    if let Some(a) = f.archived {
        let p = next(Value::Integer(a as i64), &mut values);
        sql.push_str(&format!(" AND c.archived = {p}"));
    }
    if let Some(pin) = f.pinned {
        let p = next(Value::Integer(pin as i64), &mut values);
        sql.push_str(&format!(" AND c.pinned = {p}"));
    }
    (sql, values)
}

impl Replica {
    fn to_view(&self, m: RawMessage) -> Result<MessageView> {
        let bad = || Error::Storage("corrupt view row".into());
        let id = EventId::from_slice(&m.id).ok_or_else(bad)?;
        let refs: Vec<BlobRef> = payload::decode(&m.blobs).unwrap_or_default();
        let blobs = refs
            .into_iter()
            .map(|b| BlobInfo { available: self.blobs.has(&b.hash), hash: b.hash, size: b.plain_size, media_type: b.media_type, name: b.name })
            .collect();
        let me = self.me().to_hex();
        let reactions = {
            let mut stmt = self.db.prepare_cached(
                "SELECT reaction, COUNT(DISTINCT principal), MAX(principal = ?) FROM reactions WHERE target = ? GROUP BY reaction ORDER BY MIN(add_id)",
            )?;
            stmt.query_map(params![me, &m.id], |r| Ok(ReactionSummary { reaction: r.get(0)?, count: r.get::<_, i64>(1)? as u32, mine: r.get(2)? }))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let app_data = {
            let mut stmt = self.db.prepare_cached("SELECT namespace, bytes, blobs FROM app_data_view WHERE target = ? AND principal = ? ORDER BY namespace")?;
            let rows: Vec<(String, Vec<u8>, Vec<u8>)> =
                stmt.query_map(params![&m.id, m.principal], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
            rows.into_iter()
                .map(|(namespace, bytes, blobs)| AppDataView {
                    namespace,
                    bytes,
                    blobs: payload::decode::<Vec<BlobRef>>(&blobs)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|b| BlobInfo { available: self.blobs.has(&b.hash), hash: b.hash, size: b.plain_size, media_type: b.media_type, name: b.name })
                        .collect(),
                })
                // An empty write clears the namespace.
                .filter(|a| !a.bytes.is_empty() || !a.blobs.is_empty())
                .collect()
        };
        Ok(MessageView {
            id,
            scope: ScopeId::from_slice(&m.scope).ok_or_else(bad)?,
            hlc: m.hlc as u64,
            author_device: DeviceKey::from_slice(&m.author).ok_or_else(bad)?,
            author: Principal::from_hex(&m.principal).ok_or_else(bad)?,
            is_agent: m.is_agent,
            role: role_from(m.role),
            content: m.content,
            blobs,
            reply_to: m.reply_to.and_then(|b| EventId::from_slice(&b)),
            tool_call_id: m.tool_call_id,
            tool_name: m.tool_name,
            edited: m.edited,
            status: match m.status {
                delivery::PENDING => DeliveryStatus::Pending,
                delivery::REJECTED => DeliveryStatus::Rejected(m.reason.and_then(Reason::from_code).unwrap_or(Reason::NotMember)),
                _ => DeliveryStatus::Delivered,
            },
            reactions,
            app_data,
        })
    }

    pub fn message_view(&self, id: &EventId) -> Result<Option<MessageView>> {
        let row = self.db.query_row(&format!("SELECT {MESSAGE_COLUMNS} FROM messages_view WHERE id = ?"), [&id.0[..]], raw).optional()?;
        row.map(|m| self.to_view(m)).transpose()
    }

    /// Pages history newest first; each page is returned oldest first.
    pub fn messages(&self, scope: ScopeId, before: Option<Cursor>, limit: u32) -> Result<MessagePage> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<RawMessage> = match before {
            Some(c) => {
                let mut stmt = self.db.prepare_cached(&format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages_view WHERE scope = ? AND (hlc, author_device, id) < (?, ?, ?)
                     ORDER BY hlc DESC, author_device DESC, id DESC LIMIT ?"
                ))?;
                stmt.query_map(params![&scope.0[..], c.hlc as i64, &c.author.0[..], &c.id.0[..], limit + 1], raw)?.collect::<std::result::Result<_, _>>()?
            }
            None => {
                let mut stmt = self.db.prepare_cached(&format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages_view WHERE scope = ? ORDER BY hlc DESC, author_device DESC, id DESC LIMIT ?"
                ))?;
                stmt.query_map(params![&scope.0[..], limit + 1], raw)?.collect::<std::result::Result<_, _>>()?
            }
        };
        let has_more = rows.len() > limit as usize;
        let mut messages: Vec<MessageView> = rows.into_iter().take(limit as usize).map(|m| self.to_view(m)).collect::<Result<_>>()?;
        messages.reverse();
        let next_cursor = if has_more { messages.first().map(|m| Cursor { hlc: m.hlc, author: m.author_device, id: m.id }) } else { None };
        let at_sync_boundary = !has_more && !self.backfill_complete(scope)?;
        Ok(MessagePage { messages, next_cursor, at_sync_boundary })
    }

    pub fn backfill_complete(&self, scope: ScopeId) -> Result<bool> {
        let v: Option<Option<i64>> = self.db.query_row("SELECT synced_from_hlc FROM scopes WHERE scope = ?", [&scope.0[..]], |r| r.get(0)).optional()?;
        Ok(matches!(v, Some(Some(0))))
    }

    pub fn conversation(&self, scope: ScopeId) -> Result<Option<ConversationSummary>> {
        Ok(self.conversations_where("c.scope = ?1", "", params![&scope.0[..], self.me().to_hex()])?.into_iter().next())
    }

    pub fn conversations(&self) -> Result<Vec<ConversationSummary>> {
        // Scopes this user isn't a member of (removed, or offered but not joined) stay out of the list.
        self.conversations_where("c.my_role IS NOT NULL AND ?1 IS NULL", "", params![Option::<Vec<u8>>::None, self.me().to_hex()])
    }

    /// The conversations matching `f`, in list order.
    pub fn conversations_filtered(&self, f: &ConversationFilter) -> Result<Vec<ConversationSummary>> {
        let (clause, mut values) = filter_sql(f, "c.scope", 3);
        let tail = match (f.limit, f.offset) {
            (0, 0) => String::new(),
            (0, o) => format!(" LIMIT -1 OFFSET {o}"),
            (l, o) => format!(" LIMIT {l} OFFSET {o}"),
        };
        let mut all: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Null, self.me().to_hex().into()];
        all.append(&mut values);
        self.conversations_where(&format!("c.my_role IS NOT NULL AND ?1 IS NULL{clause}"), &tail, rusqlite::params_from_iter(all))
    }

    fn conversations_where(&self, filter: &str, tail: &str, p: impl rusqlite::Params) -> Result<Vec<ConversationSummary>> {
        // An imported conversation's messages keep their original times while its genesis is
        // stamped at import, so activity is the newest message, and the genesis only when empty.
        let sql = format!(
            "SELECT c.scope, c.kind, c.title, c.pinned, c.sort_order, c.archived, c.my_role, c.members,
                COALESCE((SELECT MAX(hlc) FROM messages_view m WHERE m.scope = c.scope), c.created_hlc) AS activity,
                (SELECT content FROM messages_view m WHERE m.scope = c.scope ORDER BY hlc DESC, author_device DESC, id DESC LIMIT 1),
                (SELECT COUNT(*) FROM messages_view m WHERE m.scope = c.scope AND m.principal != ?2
                    AND m.hlc > COALESCE((SELECT last_read_hlc FROM read_state r WHERE r.scope = c.scope), 0)),
                COALESCE((SELECT replication FROM scopes s WHERE s.scope = c.scope), 0),
                c.created_ms,
                (SELECT COUNT(*) FROM messages_view m WHERE m.scope = c.scope),
                c.app_data
             FROM conversations_view c WHERE {filter}
             ORDER BY c.pinned DESC, c.sort_order IS NULL, c.sort_order, activity DESC{tail}"
        );
        let mut stmt = self.db.prepare_cached(&sql)?;
        let rows = stmt.query_map(p, |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<Vec<u8>>>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, i64>(10)?,
                (r.get::<_, i64>(11)?, r.get::<_, i64>(12)?, r.get::<_, i64>(13)?, r.get::<_, Option<Vec<u8>>>(14)?),
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (scope, kind, title, pinned, sort_order, archived, my_role, members, activity, preview, unread, (repl, created_ms, count, app_data)) = row?;
            let members: Vec<(String, i64)> = members.and_then(|b| payload::decode(&b).ok()).unwrap_or_default();
            let tags: Vec<(String, String)> = {
                let mut stmt = self.db.prepare_cached("SELECT key, value FROM conversation_tags WHERE scope = ? ORDER BY key")?;
                stmt.query_map([&scope], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
            };
            out.push(ConversationSummary {
                replication: if repl == replication::DEVICE_LOCAL { Replication::DeviceLocal } else { Replication::Synced },
                created_ms: created_ms as u64,
                message_count: count as u32,
                tags,
                app_data: app_data.and_then(|b| payload::decode(&b).ok()).unwrap_or_default(),
                scope: ScopeId::from_slice(&scope).ok_or_else(|| Error::Storage("bad scope".into()))?,
                kind: if kind == 1 { ScopeKind::Workspace } else { ScopeKind::Conversation },
                title,
                pinned,
                sort_order,
                archived,
                last_activity_hlc: activity as u64,
                last_preview: preview.map(|p| p.chars().take(140).collect()),
                unread: unread as u32,
                my_role: my_role.and_then(|r| Role::from_u8(r as u8)),
                members: members
                    .into_iter()
                    .filter_map(|(p, r)| Some(MemberInfo { principal: Principal::from_hex(&p)?, role: Role::from_u8(r as u8)? }))
                    .collect(),
            });
        }
        Ok(out)
    }

    /// Full-text search over decrypted, synced history.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.search_filtered(query, &ConversationFilter::default(), limit)
    }

    /// Full-text search within the conversations matching `f` (its paging fields are ignored).
    pub fn search_filtered(&self, query: &str, f: &ConversationFilter, limit: u32) -> Result<Vec<SearchHit>> {
        let terms: Vec<String> = query.split_whitespace().map(|t| t.replace('"', "")).filter(|t| !t.is_empty()).map(|t| format!("\"{t}\"*")).collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let (clause, mut values) = filter_sql(f, "m.scope", 3);
        let sql = format!(
            "SELECT m.scope, m.id, snippet(messages_fts, 0, '[', ']', '…', 12), m.hlc
             FROM messages_fts JOIN messages_view m ON m.rowid = messages_fts.rowid
             LEFT JOIN conversations_view c ON c.scope = m.scope
             WHERE messages_fts MATCH ?1 AND ?2 IS NOT NULL{clause} ORDER BY rank LIMIT {}",
            limit.clamp(1, 200)
        );
        let mut all: Vec<rusqlite::types::Value> = vec![terms.join(" ").into(), 1i64.into()];
        all.append(&mut values);
        let mut stmt = self.db.prepare_cached(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(all), |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (s, id, snippet, hlc) = row?;
            if let (Some(scope), Some(id)) = (ScopeId::from_slice(&s), EventId::from_slice(&id)) {
                out.push(SearchHit { scope, id, snippet, hlc: hlc as u64 });
            }
        }
        Ok(out)
    }

    /// Every certified device of this user, and of any member user, that this device knows.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT k.device, k.user, k.name, k.platform, k.last_seen_ms, EXISTS(SELECT 1 FROM revoked_devices r WHERE r.device = k.device)
             FROM known_devices k WHERE k.user = ? ORDER BY k.name",
        )?;
        let me = self.device_key();
        let rows = stmt.query_map([&self.user_key().0[..]], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, bool>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (d, u, name, platform, seen, revoked) = row?;
            let (Some(device), Some(user)) = (DeviceKey::from_slice(&d), UserKey::from_slice(&u)) else { continue };
            out.push(DeviceInfo { device, user, name, platform, last_seen_ms: seen.map(|s| s as u64), revoked, is_self: device == me });
        }
        Ok(out)
    }

    pub fn device_name(&self, device: &DeviceKey) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT name FROM known_devices WHERE device = ? LIMIT 1", [&device.0[..]], |r| r.get(0)).optional()?)
    }

    /// Finds the blob reference (including its key) for a blob attached in a scope.
    pub fn blob_ref(&self, scope: ScopeId, hash: &[u8; 32]) -> Result<Option<BlobRef>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT m.blobs FROM blob_refs b JOIN messages_view m ON m.id = b.event WHERE b.hash = ?1 AND b.scope = ?2
             UNION ALL SELECT a.blobs FROM blob_refs b JOIN app_data_view a ON a.target = b.event WHERE b.hash = ?1 AND b.scope = ?2",
        )?;
        let rows: Vec<Vec<u8>> = stmt.query_map(params![&hash[..], &scope.0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        for b in rows {
            if let Ok(refs) = payload::decode::<Vec<BlobRef>>(&b)
                && let Some(r) = refs.into_iter().find(|r| r.hash == *hash)
            {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// Path to a decrypted copy of a locally available blob.
    pub fn blob_path(&self, scope: ScopeId, hash: &[u8; 32]) -> Result<Option<std::path::PathBuf>> {
        let Some(r) = self.blob_ref(scope, hash)? else { return Err(Error::NotFound("blob".into())) };
        if !self.blobs.has(hash) {
            return Ok(None);
        }
        Ok(Some(self.blobs.open_plain_capped(&r, self.cfg.plain_cache_bytes)?))
    }

    /// The decrypted bytes of a locally available blob, without a plaintext copy on disk.
    pub fn blob_bytes(&self, scope: ScopeId, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let Some(r) = self.blob_ref(scope, hash)? else { return Err(Error::NotFound("blob".into())) };
        if !self.blobs.has(hash) {
            return Ok(None);
        }
        Ok(Some(self.blobs.read_plain(&r)?))
    }

    pub fn blob_store(&self) -> &crate::blobs::BlobStore {
        &self.blobs
    }

    pub fn mark_blob_available(&mut self, hash: &[u8; 32]) -> Result<()> {
        self.db.execute("UPDATE blobs SET available = 1 WHERE hash = ?", [&hash[..]])?;
        let ids: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT event FROM blob_refs WHERE hash = ?")?;
            stmt.query_map([&hash[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        for id in ids {
            if let Some(id) = EventId::from_slice(&id) {
                self.emit_message_changed(&id)?;
            }
        }
        self.changes.append(&mut self.pending_changes);
        Ok(())
    }

    pub fn pending_outbox(&self, scope: ScopeId) -> Result<u32> {
        Ok(self.db.query_row("SELECT COUNT(*) FROM outbox WHERE scope = ?", [&scope.0[..]], |r| r.get::<_, i64>(0))? as u32)
    }

    pub fn last_sync_ms(&self, scope: ScopeId) -> Result<Option<u64>> {
        let v: Option<Option<i64>> = self.db.query_row("SELECT last_sync_ms FROM scopes WHERE scope = ?", [&scope.0[..]], |r| r.get(0)).optional()?;
        Ok(v.flatten().map(|v| v as u64))
    }

    pub fn synced_from_hlc(&self, scope: ScopeId) -> Result<Option<u64>> {
        let v: Option<Option<i64>> = self.db.query_row("SELECT synced_from_hlc FROM scopes WHERE scope = ?", [&scope.0[..]], |r| r.get(0)).optional()?;
        Ok(v.flatten().map(|v| v as u64))
    }
}
