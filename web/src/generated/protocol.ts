// Generated from crates/uplink-protocol by its `typescript` example; do not edit.

/**
 * Id of a log entry. Entries are numbered from 1 in commit order, so an
 * id also serves as a position in the event stream; 0 is the position
 * before the first entry.
 */
export type TxLogId = number;

export type WsClientMsg = { "type": "ping", seq: number, } | { "type": "hello", 
/**
 * The last entry the client received on a previous connection, if
 * it wants to resume rather than load afresh.
 */
since: TxLogId | null, } | { "type": "request", request_id: number, body: ClientRequest, };

export type WsServerMsg = { "type": "pong", seq: number, } | { "type": "welcome", 
/**
 * The most recent entry at the time of `Hello`.
 */
cursor: TxLogId, resumed: boolean, } | { "type": "response", request_id: number, body: ServerResponse, } | { "type": "request_failed", request_id: number, error: ServerError, } | { "type": "entry", entry: LogEntry, };

export type ClientRequest = { "type": "undo", tx_log_id: TxLogId, } | { "type": "list_history", before: TxLogId | null, limit: number, } | { "type": "list_accounts" } | { "type": "list_entries", list: EntryList, after: ListPosition | null, limit: number, } | { "type": "search", query: string, limit: number, } | { "type": "list_conversation", entry: EntryId, } | { "type": "list_message_entries", message: MessageId, } | { "type": "load_message", message: MessageId, } | { "type": "modify_labels", entries: Array<EntryId>, add: Array<LabelId>, remove: Array<LabelId>, } | { "type": "list_drafts" } | { "type": "create_draft", id: DraftId, account: string, content: DraftContent, in_reply_to: MessageId | null, } | { "type": "edit_draft", draft: DraftId, revision: number, splice: Splice, } | { "type": "delete_draft", draft: DraftId, } | { "type": "attach_to_draft", draft: DraftId, blob: BlobId, name: string, mime: string, } | { "type": "detach_from_draft", draft: DraftId, attachment: DraftAttachmentId, } | { "type": "send_draft", draft: DraftId, } | { "type": "cancel_send", item: OutboxId, } | { "type": "keep_sent", item: OutboxId, } | { "type": "list_outbox" } | { "type": "keep_observed", account: string, op: TxLogId | null, } | { "type": "list_rooms" } | { "type": "list_timeline", room: RoomId, page: TimelinePage, limit: number, } | { "type": "list_room_events", room: RoomId, events: Array<RoomEventId>, } | { "type": "list_edits", room: RoomId, event: RoomEventId, } | { "type": "list_matrix_sends", room: RoomId, } | { "type": "send_matrix_message", room: RoomId, body: string, } | { "type": "cancel_matrix_send", item: MatrixSendId, } | { "type": "keep_matrix_sent", item: MatrixSendId, } | { "type": "mark_room_read", room: RoomId, event: RoomEventId, } | { "type": "change_membership", room: RoomId, change: MembershipChange, } | { "type": "list_events", from: number, to: number, } | { "type": "list_todos" } | { "type": "create_todo", title: string, notes: string, due: string | null, message: MessageId | null, } | { "type": "edit_todo", todo: TodoId, edit: TodoEdit, } | { "type": "delete_todo", todo: TodoId, };

export type ServerResponse = { "type": "undone", tx_log_id: TxLogId, } | { "type": "dispatched", tx_log_id: TxLogId, } | { "type": "accounts", as_of: TxLogId, accounts: Array<Account>, } | { "type": "conversations", as_of: TxLogId, conversations: Array<Conversation>, 
/**
 * Where the next page starts, if there may be one.
 */
next: ListPosition | null, } | { "type": "entries", as_of: TxLogId, entries: Array<Entry>, } | { "type": "message", message: MessageContent, } | { "type": "drafts", as_of: TxLogId, drafts: Array<Draft>, } | { "type": "outbox", as_of: TxLogId, accounts: Array<AccountOutbox>, } | { "type": "rooms", as_of: TxLogId, accounts: Array<MatrixAccount>, rooms: Array<Room>, } | { "type": "timeline", as_of: TxLogId, room: RoomId, events: Array<RoomEvent>, 
/**
 * Whether there are earlier events than those listed.
 */
more_before: boolean, } | { "type": "room_events", as_of: TxLogId, room: RoomId, events: Array<RoomEvent>, } | { "type": "edits", as_of: TxLogId, room: RoomId, event: RoomEventId, edits: Array<RoomEvent>, } | { "type": "matrix_sends", as_of: TxLogId, room: RoomId, sends: Array<MatrixSend>, } | { "type": "todos", as_of: TxLogId, todos: Array<Todo>, } | { "type": "events", as_of: TxLogId, events: Array<CalendarEvent>, } | { "type": "history", as_of: TxLogId, entries: Array<HistoryEntry>, 
/**
 * Whether there are earlier ones than those listed.
 */
more: boolean, };

export type ServerError = { code: ErrorCode, message: string, };

export type ErrorCode = "not_found" | "not_undoable" | "already_undone" | "conflict" | "invalid" | "internal";

export type LogEntry = { id: TxLogId, 
/**
 * Milliseconds since the Unix epoch.
 */
ts: number, source: Source, description: string, op_type: string, 
/**
 * Set if this entry is the undo of entry `undoes`, which is thereby
 * undone.
 */
undoes: TxLogId | null, 
/**
 * What the entry changed, for clients to apply to their state.
 */
events: Array<ServerEvent>, };

/**
 * What a message list is of.
 */
export type EntryList = { "type": "label", label: LabelId, } | { "type": "all_mail", account: string, } | { "type": "inbox" };

/**
 * A user's op as the history lists it.
 */
export type HistoryEntry = { entry: LogEntry, 
/**
 * The entry that undid it, if it was undone.
 */
undone_by: TxLogId | null, };

export type Source = { "type": "user" } | { "type": "account", id: string, };

/**
 * A change to state that clients mirror (SPEC §5.4). Events carry ids
 * rather than content, since they are stored in the log; clients query
 * what they display.
 */
export type ServerEvent = { "type": "entries_added" } & EntriesAdded | { "type": "entries_removed" } & EntriesRemoved | { "type": "labels_changed" } & LabelsChanged | { "type": "label_list_changed" } & LabelListChanged | { "type": "draft_edited" } & DraftEdited | { "type": "drafts_changed" } & DraftsChanged | { "type": "outbox_changed" } & OutboxChanged | { "type": "rooms_changed" } & RoomsChanged | { "type": "timeline_added" } & TimelineAdded | { "type": "timeline_changed" } & TimelineChanged | { "type": "matrix_sends_changed" } & MatrixSendsChanged | { "type": "todos_changed" } & TodosChanged | { "type": "calendar_changed" } & CalendarChanged;

export type MessageId = number;

export type EntryId = number;

export type LabelId = number;

export type Account = { id: string, address: string, provider: MailProvider, 
/**
 * Whether the first full sync has completed.
 */
synced: boolean, 
/**
 * Until then, how far it has got, if the provider says how far it
 * goes.
 */
listing: ListingProgress | null, labels: Array<Label>, };

/**
 * Who keeps a mail account.
 */
export type MailProvider = "gmail" | "m365";

export type Label = { id: LabelId, 
/**
 * The provider's id, e.g. `INBOX` or `Label_12`.
 */
external_id: string, name: string, 
/**
 * Defined by the provider rather than the user.
 */
system: boolean, 
/**
 * One of the account's folders, of which an entry is in exactly one.
 */
folder: boolean, 
/**
 * Conversations with an entry carrying the label (as desired).
 */
total: number, 
/**
 * Of those, the ones with an entry carrying it and `UNREAD` too.
 */
unread: number, };

/**
 * An entry as shown in a message list.
 */
export type Entry = { id: EntryId, account: string, message: MessageId, thread_id: string | null, 
/**
 * When the account received it, ms since the Unix epoch.
 */
received_at: number, subject: string | null, from_addr: string | null, from_name: string | null, snippet: string, 
/**
 * Whether it has attachments other than images its HTML body shows.
 */
has_attachments: boolean, 
/**
 * Labels as desired, i.e. including changes not yet pushed.
 */
labels: Array<LabelId>, };

/**
 * A conversation as a list shows it: the entries of one account's
 * thread (Gmail's `threadId`, Microsoft 365's `conversationId`), or an
 * entry without one by itself, as far as the list's scope goes: the
 * entries in Trash, in a list of Trash; else those not in Trash or Spam.
 */
export type Conversation = { 
/**
 * Its newest entry: the row shows it, and the list is ordered by it.
 */
latest: Entry, 
/**
 * Its other entries, oldest first.
 */
earlier: Array<Member>, 
/**
 * Who sent its entries, each once, in the order they first did.
 */
senders: Array<Sender>, 
/**
 * Whether any of its entries has attachments.
 */
has_attachments: boolean, };

export type ListingProgress = { 
/**
 * Messages stored.
 */
stored: number, 
/**
 * Messages the mailbox held as the listing began, Spam and Trash
 * included, as the listing does.
 */
total: number, };

/**
 * An entry of a conversation other than its latest.
 */
export type Member = { id: EntryId, message: MessageId, 
/**
 * Labels as desired.
 */
labels: Array<LabelId>, };

export type Sender = { name: string | null, addr: string | null, };

/**
 * Where a page of a message list ends: its last conversation's latest
 * entry. Lists run newest first.
 */
export type ListPosition = { received_at: number, entry: EntryId, };

export type MessageContent = { id: MessageId, message_id: string, subject: string | null, from_addr: string | null, from_name: string | null, to_addrs: Array<string>, cc_addrs: Array<string>, 
/**
 * The `Date:` header, unix seconds.
 */
date_sent: number | null, body_text: string, 
/**
 * The HTML body, sanitized, if the message has one. It may refer to
 * remote resources, which clients should not load unasked.
 */
body_html: string | null, attachments: Array<Attachment>, };

/**
 * A part of a message other than its body. Its content is had over HTTP:
 * `/api/messages/{message}/attachments/{index}`.
 */
export type Attachment = { index: number, 
/**
 * The file name the message gives it, if any.
 */
name: string | null, 
/**
 * `type/subtype`.
 */
mime: string, 
/**
 * Decoded, in bytes.
 */
size: number, 
/**
 * An image the HTML body shows, rather than a file attached.
 */
inline: boolean, };

export type EntriesAdded = { entries: Array<EntryId>, };

export type EntriesRemoved = { entries: Array<EntryId>, };

export type LabelsChanged = { changes: Array<LabelState>, };

/**
 * An entry's membership of a label after a change.
 */
export type LabelState = { entry: EntryId, label: LabelId, desired: boolean, observed: boolean, };

/**
 * An account's labels were created, renamed or deleted.
 */
export type LabelListChanged = { account: string, };

/**
 * Chosen by the client creating the draft (a UUID), so that it can be
 * edited before the server has answered.
 */
export type DraftId = string;

export type OutboxId = number;

export type DraftContent = { 
/**
 * Address lists as typed, e.g. `Bob <bob@example.com>, carol@example.com`.
 */
to: string, cc: string, bcc: string, subject: string, 
/**
 * HTML, as the editor produces it.
 */
body: string, };

/**
 * A file uploaded to be attached (`POST /api/blobs`, SPEC §7.4): the
 * lowercase hex SHA-256 of its content.
 */
export type BlobId = string;

/**
 * The answer to `POST /api/blobs`.
 */
export type UploadedBlob = { blob: BlobId, 
/**
 * In bytes.
 */
size: number, };

export type DraftAttachmentId = number;

/**
 * A file attached to a draft.
 */
export type DraftAttachment = { id: DraftAttachmentId, blob: BlobId, 
/**
 * The file name it is sent with.
 */
name: string, mime: string, 
/**
 * In bytes.
 */
size: number, };

export type Draft = { id: DraftId, 
/**
 * The account it is sent from.
 */
account: string, 
/**
 * Number of edits applied since creation.
 */
revision: number, content: DraftContent, 
/**
 * In the order attached. Not part of `content`: attaching and
 * detaching are ops of their own, and leave the revision as it is.
 */
attachments: Array<DraftAttachment>, 
/**
 * The message this replies to.
 */
in_reply_to: MessageId | null, 
/**
 * Ms since the Unix epoch.
 */
updated_at: number, };

export type DraftField = "to" | "cc" | "bcc" | "subject" | "body";

/**
 * Replaces `delete` at `offset` of `field` with `insert`.
 */
export type Splice = { field: DraftField, 
/**
 * In UTF-16 code units.
 */
offset: number, delete: string, insert: string, };

/**
 * Where a mail being sent is (SPEC §6.7).
 */
export type SendState = "queued" | "sending" | "unknown" | "sent" | "refused";

/**
 * A mail being sent, or sent and not yet synced back, or sent though no
 * longer wanted (needing a decision).
 */
export type OutboxItem = { id: OutboxId, account: string, draft: Draft, state: SendState, 
/**
 * Whether it is to be sent: `false` once cancelled while it could no
 * longer be taken back for sure. Sent, it needs a decision: keeping
 * it sent.
 */
wanted: boolean, error: string | null, 
/**
 * When it is sent, ms since the Unix epoch.
 */
not_before: number, };

/**
 * What of an account is not yet reconciled with its provider (SPEC §6.6).
 */
export type AccountOutbox = { account: string, 
/**
 * Why its changes are not reaching the provider, while they are not.
 */
failing: Failing | null, pending: Pending, };

/**
 * What is pending, by the kind of account.
 */
export type Pending = { "kind": "mail", 
/**
 * Label changes pending, by the op that made them, newest first.
 */
changes: Array<PendingChanges>, 
/**
 * Mail being sent, sent and not yet synced back, or sent though
 * cancelled.
 */
sends: Array<OutboxItem>, } | { "kind": "matrix", 
/**
 * Messages being sent, sent and not yet synced back, or sent
 * though cancelled.
 */
sends: Array<MatrixSend>, 
/**
 * Rooms being joined or left, by the op that asked, newest first.
 */
memberships: Array<PendingMembership>, 
/**
 * Rooms read past their latest read receipt.
 */
receipts: Array<RoomId>, };

export type Failing = { error: string, 
/**
 * Ms since the Unix epoch.
 */
since: number, };

/**
 * The label changes of one op not yet in effect at the provider.
 */
export type PendingChanges = { 
/**
 * The op, with what it did and when; `None` for changes made before
 * ops were recorded with them.
 */
op: PendingOp | null, 
/**
 * The messages with changes still to push.
 */
entries: number, 
/**
 * Of those, the ones the provider refused, by its reason.
 */
refused: Array<Refusal>, };

export type PendingOp = { id: TxLogId, description: string, 
/**
 * Ms since the Unix epoch.
 */
ts: number, };

export type Refusal = { reason: string, entries: number, };

/**
 * A draft's text changed by `splice`, making its revision `revision`.
 */
export type DraftEdited = { draft: DraftId, revision: number, splice: Splice, };

/**
 * Drafts were created or deleted.
 */
export type DraftsChanged = { drafts: Array<DraftId>, };

/**
 * Something not yet reconciled in these accounts was added, changed
 * state or went (SPEC §6.6).
 */
export type OutboxChanged = { accounts: Array<string>, };

/**
 * A room of one account, as uplink numbers it.
 */
export type RoomId = number;

/**
 * A timeline event, as uplink numbers it: in the order sync delivered
 * events, so also the timeline's order.
 */
export type RoomEventId = number;

export type MatrixSendId = number;

export type MatrixAccount = { id: string, mxid: string, 
/**
 * Whether the first sync has completed.
 */
synced: boolean, };

export type Membership = "joined" | "invited" | "left";

/**
 * Joining a room one is invited to, or leaving (or declining) one.
 */
export type MembershipChange = "join" | "leave";

export type Room = { id: RoomId, account: string, 
/**
 * The Matrix room id, `!opaque:server`.
 */
room_id: string, name: string, encrypted: boolean, membership: Membership, 
/**
 * Messages from others after the read marker (counting those that
 * could not be decrypted).
 */
unread: number, 
/**
 * The latest event's time, ms since the Unix epoch.
 */
last_event_ts: number | null, read_up_to: RoomEventId | null, 
/**
 * A change the user asked for that has yet to take effect.
 */
pending: MembershipChange | null, 
/**
 * Why the homeserver refused the last change asked for.
 */
membership_error: string | null, 
/**
 * Whether the room is a space.
 */
space: boolean, 
/**
 * `mxc://server/media`: the room's avatar, or of a direct chat without
 * one, the other user's. Served at
 * `/api/rooms/{id}/avatar/{server}/{media}`.
 */
avatar: string | null, 
/**
 * Of a space, the Matrix ids of the rooms it lists, joined or not, in
 * the space's order.
 */
children: Array<string>, };

export type Encryption = "none" | "decrypted" | "undecryptable";

export type RoomEvent = { id: RoomEventId, 
/**
 * The Matrix event id, `$opaque`.
 */
event_id: string, sender: string, sender_name: string | null, 
/**
 * `origin_server_ts`, ms since the Unix epoch.
 */
ts: number, 
/**
 * The event type, e.g. `m.room.message`.
 */
kind: string, 
/**
 * Of `m.room.message`: `m.text`, `m.notice`, `m.image`, ….
 */
msgtype: string | null, 
/**
 * Of `m.room.message`: the plain-text body.
 */
body: string | null, 
/**
 * The event as JSON, decrypted if it could be, for event types
 * uplink does not display otherwise.
 */
raw: string, encryption: Encryption, 
/**
 * Whether the event is redacted: `raw` is as redacted, with
 * `unsigned.redacted_because`.
 */
redacted: boolean, 
/**
 * Of a message, its latest edit by its sender.
 */
edit: EventEdit | null, 
/**
 * The reactions to it, by key, in the order first reacted with.
 */
reactions: Array<Reaction>, 
/**
 * Of a reply (`m.in_reply_to`), the event it answers, if stored.
 */
replied: RepliedEvent | null, };

/**
 * An edit (`m.replace`) of a message.
 */
export type EventEdit = { event_id: string, 
/**
 * Ms since the Unix epoch.
 */
ts: number, 
/**
 * The message's content as edited (`m.new_content`), as JSON.
 */
content: string, };

export type Reaction = { 
/**
 * What is reacted with: usually an emoji.
 */
key: string, 
/**
 * Each sender once, in the order they reacted.
 */
senders: Array<Reactor>, };

export type Reactor = { sender: string, sender_name: string | null, 
/**
 * The reaction's event: redacting it takes the reaction back.
 */
event_id: string, };

/**
 * The event a reply answers, as the reply shows it.
 */
export type RepliedEvent = { id: RoomEventId, event_id: string, sender: string, sender_name: string | null, kind: string, 
/**
 * As last edited.
 */
msgtype: string | null, 
/**
 * As last edited.
 */
body: string | null, redacted: boolean, };

/**
 * Which part of a room's timeline to list.
 */
export type TimelinePage = { "type": "latest" } | { "type": "before", event: RoomEventId, } | { "type": "after", event: RoomEventId, };

/**
 * Where a Matrix message being sent is (SPEC §9.3). Never unknown: the
 * homeserver deduplicates the transaction id, so it is sent again.
 */
export type MatrixSendState = "queued" | "sending" | "sent" | "refused";

/**
 * A message being sent, sent and not yet synced back, or sent though
 * cancelled (needing a decision).
 */
export type MatrixSend = { id: MatrixSendId, room: RoomId, body: string, state: MatrixSendState, 
/**
 * Whether it is to be sent: `false` once cancelled while being
 * sent. Sent, it needs a decision: keeping it sent.
 */
wanted: boolean, error: string | null, 
/**
 * Ms since the Unix epoch.
 */
queued_at: number, };

/**
 * A room being joined or left (SPEC §9.7).
 */
export type PendingMembership = { room: RoomId, change: MembershipChange, 
/**
 * The op that asked for it: undoing it keeps the room as it is.
 */
op: PendingOp, 
/**
 * Why the homeserver refused it: it needs a decision, which undoing
 * the op makes.
 */
refused: string | null, };

/**
 * Rooms appeared, or their name, membership, avatar, children, unread
 * count or read marker changed.
 */
export type RoomsChanged = { rooms: Array<RoomId>, };

/**
 * Events were added to the end of a room's timeline.
 */
export type TimelineAdded = { room: RoomId, events: Array<RoomEventId>, };

/**
 * What these events of the room show changed: they were edited,
 * reacted to or redacted, or the event they reply to was.
 */
export type TimelineChanged = { room: RoomId, events: Array<RoomEventId>, };

/**
 * Messages being sent to these rooms were added, changed state or went.
 */
export type MatrixSendsChanged = { rooms: Array<RoomId>, };

export type TodoId = number;

export type Todo = { id: TodoId, title: string, notes: string, 
/**
 * A date, `YYYY-MM-DD`.
 */
due: string | null, 
/**
 * When it was done, ms since the Unix epoch; `None` while open.
 */
done_at: number | null, 
/**
 * The message it was flagged from, which is flagged while it is open.
 */
message: MessageId | null, 
/**
 * Ms since the Unix epoch.
 */
created_at: number, };

/**
 * A change to one of a to-do's fields.
 */
export type TodoEdit = { "field": "title", title: string, } | { "field": "notes", notes: string, } | { "field": "due", due: string | null, } | { "field": "done", done: boolean, };

/**
 * To-dos were added, removed or changed.
 */
export type TodosChanged = { todos: Array<TodoId>, };

export type EventId = number;

/**
 * An event as the calendar shows it: a single event, or one occurrence
 * of a series.
 */
export type CalendarEvent = { id: EventId, account: string, subject: string, location: string, 
/**
 * The start of the body, as text.
 */
preview: string, 
/**
 * Ms since the Unix epoch. An all-day event's start and end are UTC
 * midnights, of its first day and of the day after its last.
 */
start: number, end: number, all_day: boolean, 
/**
 * One occurrence of a series.
 */
recurring: boolean, 
/**
 * An occurrence of a series changed from what the series says.
 */
exception: boolean, };

/**
 * An account's calendar changed.
 */
export type CalendarChanged = { account: string, };
