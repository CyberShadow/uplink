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

export type ClientRequest = { "type": "undo", tx_log_id: TxLogId, } | { "type": "list_history", before: TxLogId | null, limit: number, } | { "type": "list_accounts" } | { "type": "list_entries", list: EntryList, after: ListPosition | null, limit: number, } | { "type": "search", query: string, limit: number, } | { "type": "list_message_entries", message: MessageId, } | { "type": "load_message", message: MessageId, } | { "type": "modify_labels", entries: Array<EntryId>, add: Array<LabelId>, remove: Array<LabelId>, } | { "type": "list_drafts" } | { "type": "create_draft", id: DraftId, account: string, content: DraftContent, in_reply_to: MessageId | null, } | { "type": "edit_draft", draft: DraftId, revision: number, splice: Splice, } | { "type": "delete_draft", draft: DraftId, } | { "type": "attach_to_draft", draft: DraftId, blob: BlobId, name: string, mime: string, } | { "type": "detach_from_draft", draft: DraftId, attachment: DraftAttachmentId, } | { "type": "send_draft", draft: DraftId, } | { "type": "cancel_send", item: OutboxId, } | { "type": "list_outbox" } | { "type": "list_rooms" } | { "type": "list_timeline", room: RoomId, page: TimelinePage, limit: number, } | { "type": "list_matrix_outbox" } | { "type": "send_matrix_message", room: RoomId, body: string, } | { "type": "cancel_matrix_send", item: MatrixOutboxId, } | { "type": "mark_room_read", room: RoomId, event: RoomEventId, } | { "type": "change_membership", room: RoomId, change: MembershipChange, } | { "type": "list_events", from: number, to: number, } | { "type": "list_todos" } | { "type": "create_todo", title: string, notes: string, due: string | null, message: MessageId | null, } | { "type": "edit_todo", todo: TodoId, edit: TodoEdit, } | { "type": "delete_todo", todo: TodoId, };

export type ServerResponse = { "type": "undone", tx_log_id: TxLogId, } | { "type": "dispatched", tx_log_id: TxLogId, } | { "type": "accounts", as_of: TxLogId, accounts: Array<Account>, } | { "type": "entries", as_of: TxLogId, entries: Array<Entry>, 
/**
 * Where the next page starts, if there may be one.
 */
next: ListPosition | null, } | { "type": "message", message: MessageContent, } | { "type": "drafts", as_of: TxLogId, drafts: Array<Draft>, } | { "type": "outbox", as_of: TxLogId, items: Array<OutboxItem>, } | { "type": "rooms", as_of: TxLogId, accounts: Array<MatrixAccount>, rooms: Array<Room>, } | { "type": "timeline", as_of: TxLogId, room: RoomId, events: Array<RoomEvent>, 
/**
 * Whether there are earlier events than those listed.
 */
more_before: boolean, } | { "type": "matrix_outbox", as_of: TxLogId, items: Array<MatrixOutboxItem>, } | { "type": "todos", as_of: TxLogId, todos: Array<Todo>, } | { "type": "events", as_of: TxLogId, events: Array<CalendarEvent>, } | { "type": "history", as_of: TxLogId, entries: Array<HistoryEntry>, 
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
export type ServerEvent = { "type": "entries_added" } & EntriesAdded | { "type": "entries_removed" } & EntriesRemoved | { "type": "labels_changed" } & LabelsChanged | { "type": "label_list_changed" } & LabelListChanged | { "type": "draft_edited" } & DraftEdited | { "type": "drafts_changed" } & DraftsChanged | { "type": "outbox_changed" } & OutboxChanged | { "type": "rooms_changed" } & RoomsChanged | { "type": "timeline_added" } & TimelineAdded | { "type": "matrix_outbox_changed" } & MatrixOutboxChanged | { "type": "todos_changed" } & TodosChanged | { "type": "calendar_changed" } & CalendarChanged;

export type MessageId = number;

export type EntryId = number;

export type LabelId = number;

export type Account = { id: string, address: string, provider: MailProvider, 
/**
 * Whether the first full sync has completed.
 */
synced: boolean, labels: Array<Label>, };

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
 * Entries carrying the label (as desired).
 */
total: number, 
/**
 * Of those, the ones also labelled `UNREAD`.
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
 * Where a page of a message list ends; lists run newest first.
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

export type OutboxState = "queued" | "sending" | "sent" | "failed";

export type OutboxItem = { id: OutboxId, account: string, draft: Draft, state: OutboxState, error: string | null, 
/**
 * When it is sent, ms since the Unix epoch.
 */
not_before: number, };

/**
 * A draft's text changed by `splice`, making its revision `revision`.
 */
export type DraftEdited = { draft: DraftId, revision: number, splice: Splice, };

/**
 * Drafts were created or deleted.
 */
export type DraftsChanged = { drafts: Array<DraftId>, };

/**
 * Outbox items were added, removed or changed state.
 */
export type OutboxChanged = { items: Array<OutboxId>, };

/**
 * A room of one account, as uplink numbers it.
 */
export type RoomId = number;

/**
 * A timeline event, as uplink numbers it: in the order sync delivered
 * events, so also the timeline's order.
 */
export type RoomEventId = number;

export type MatrixOutboxId = number;

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
membership_error: string | null, };

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
raw: string, encryption: Encryption, };

/**
 * Which part of a room's timeline to list.
 */
export type TimelinePage = { "type": "latest" } | { "type": "before", event: RoomEventId, } | { "type": "after", event: RoomEventId, };

export type MatrixOutboxItem = { id: MatrixOutboxId, room: RoomId, body: string, state: OutboxState, error: string | null, 
/**
 * Ms since the Unix epoch.
 */
queued_at: number, };

/**
 * Rooms appeared, or their name, membership, unread count or read
 * marker changed.
 */
export type RoomsChanged = { rooms: Array<RoomId>, };

/**
 * Events were added to the end of a room's timeline.
 */
export type TimelineAdded = { room: RoomId, events: Array<RoomEventId>, };

/**
 * Matrix outbox items were added, removed or changed state.
 */
export type MatrixOutboxChanged = { items: Array<MatrixOutboxId>, };

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
