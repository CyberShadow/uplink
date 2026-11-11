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
cursor: TxLogId, resumed: boolean, } | { "type": "response", request_id: number, body: ServerResponse, } | { "type": "request_failed", request_id: number, error: ServerError, } | { "type": "entry", entry: LogEntry, } | { "type": "notice", notice: Notice, };

/**
 * A notice of the moment (SPEC §5.5).
 */
export type Notice = { "type": "typing" } & Typing | { "type": "receipts_changed" } & ReceiptsChanged | { "type": "security_changed" } & SecurityChanged;

/**
 * Who is typing in the room, but the account: all of them, each time.
 */
export type Typing = { room: RoomId, users: Array<RoomMember>, };

export type RoomMember = { user: string, 
/**
 * Their display name in the room, if they have one.
 */
name: string | null, };

/**
 * Members of the room sent read receipts.
 */
export type ReceiptsChanged = { room: RoomId, };

/**
 * How far a member of the room (not the account) has read.
 */
export type MemberReceipt = { member: RoomMember, 
/**
 * The stored event their receipt is for, and where it is.
 */
event: RoomEventId, pos: number, 
/**
 * When they read it, ms since the Unix epoch, if the receipt says.
 */
ts: number | null, };

/**
 * A member of the room: joined, invited, left or banned.
 */
export type MemberInfo = { user: string, 
/**
 * Their display name in the room, if they have one.
 */
name: string | null, membership: MemberState, 
/**
 * Their power level in the room; `None` for a creator of a room of
 * version 12 or later, whose power is above any.
 */
power: number | null, 
/**
 * Whether the account may kick them, and ban them (unbanning needs
 * both), as its power is above theirs and the room's levels say.
 */
kickable: boolean, bannable: boolean, };

/**
 * A room to create (SPEC §9.7).
 */
export type NewRoom = { 
/**
 * Empty: none.
 */
name: string, topic: string | null, 
/**
 * Matrix user ids to invite.
 */
invite: Array<string>, encrypted: boolean, 
/**
 * The localpart of the address it is published under, as a public
 * room anyone may join; else it is private, by invitation.
 */
alias: string | null, };

export type MemberState = "joined" | "invited" | "knocking" | "left" | "banned";

/**
 * What the account may do in the room, as its power levels say.
 */
export type RoomPowers = { invite: boolean, name: boolean, topic: boolean, };

/**
 * What a send does to a member of the room.
 */
export type MemberAction = "invite" | "kick" | "ban" | "unban";

/**
 * The account's devices, their trust or its key backup changed.
 */
export type SecurityChanged = { account: string, };

/**
 * The account's device and its encryption, as its client has them
 * (SPEC §9.4).
 */
export type MatrixSecurity = { account: string, 
/**
 * uplink's device.
 */
device_id: string, 
/**
 * Whether the account's cross-signing has signed uplink's device:
 * others then trust it.
 */
verified: boolean, 
/**
 * Whether the device has the account's cross-signing keys (all
 * three), and so can verify others.
 */
cross_signing: boolean, backup: KeyBackup, recovery: Recovery, 
/**
 * The account's devices, uplink's among them, most recently seen
 * first.
 */
devices: Array<MatrixDevice>, 
/**
 * Verifications of the account's devices with uplink's, going on or
 * ended since the last began, oldest first.
 */
verifications: Array<MatrixVerification>, };

/**
 * The state of the key backup, which keeps room keys on the homeserver
 * for the account's devices (as the SDK's `BackupState`).
 */
export type KeyBackup = "unknown" | "creating" | "enabling" | "resuming" | "enabled" | "downloading" | "disabling";

/**
 * Whether the account's secrets are kept in Secret Storage, for devices
 * to recover them (as the SDK's `RecoveryState`).
 */
export type Recovery = "unknown" | "enabled" | "disabled" | "incomplete";

/**
 * A device of the account.
 */
export type MatrixDevice = { device_id: string, name: string | null, 
/**
 * When it was last seen, ms since the Unix epoch, and where from.
 */
last_seen_ts: number | null, last_seen_ip: string | null, 
/**
 * Whether uplink's device trusts it: the account's cross-signing
 * signed it.
 */
verified: boolean, 
/**
 * Whether it is uplink's.
 */
current: boolean, };

/**
 * The room's read marker: the event read up to, and where it is.
 */
export type ReadMarker = { event: RoomEventId, pos: number, };

export type GapId = number;

/**
 * Events missing from a room's timeline (SPEC §9.2), which `FillGap`
 * fetches from the homeserver.
 */
export type TimelineGap = { id: GapId, 
/**
 * The listed event the gap is just before; `None` if it is after
 * every listed event (of a sync that brought only edits, say).
 */
before: RoomEventId | null, };

/**
 * Events missing from a room's timeline were fetched: they fill `gap`,
 * which `left` replaces if they did not all come.
 */
export type TimelineFilled = { room: RoomId, gap: GapId, 
/**
 * Those listed, of the events fetched.
 */
events: Array<RoomEventId>, left: TimelineGap | null, };

/**
 * A verification of uplink's device with another of the account's, by
 * comparing emoji (SAS), as clients do (SPEC §9.4).
 */
export type MatrixVerification = { flow_id: string, 
/**
 * The other device, once known: a request uplink makes goes to every
 * device of the account, and one answers.
 */
other_device: string | null, 
/**
 * Whether uplink asked.
 */
we_started: boolean, phase: VerificationPhase, };

export type VerificationPhase = { "type": "requested" } | { "type": "ready" } | { "type": "started" } | { "type": "compare", emoji: Array<SasEmoji>, decimals: Array<number>, } | { "type": "confirmed" } | { "type": "done" } | { "type": "cancelled", reason: string, by_us: boolean, };

export type SasEmoji = { symbol: string, description: string, };

/**
 * What the user says of a verification.
 */
export type VerificationAnswer = "accept" | "confirm" | "mismatch" | "cancel";

export type ClientRequest = { "type": "undo", tx_log_id: TxLogId, } | { "type": "list_history", before: TxLogId | null, limit: number, } | { "type": "list_accounts" } | { "type": "list_entries", query: string, after: ListPosition | null, limit: number, } | { "type": "list_items", query: string, today: string, after: ListPosition | null, limit: number, } | { "type": "list_conversation", entry: EntryId, } | { "type": "list_message_entries", message: MessageId, } | { "type": "load_message", message: MessageId, } | { "type": "modify_labels", entries: Array<EntryId>, add: Array<LabelId>, remove: Array<LabelId>, } | { "type": "list_drafts" } | { "type": "create_draft", id: DraftId, account: string, content: DraftContent, in_reply_to: MessageId | null, } | { "type": "edit_draft", draft: DraftId, revision: number, splice: Splice, } | { "type": "delete_draft", draft: DraftId, } | { "type": "attach_to_draft", draft: DraftId, blob: BlobId, name: string, mime: string, } | { "type": "detach_from_draft", draft: DraftId, attachment: DraftAttachmentId, } | { "type": "send_draft", draft: DraftId, } | { "type": "cancel_send", item: OutboxId, } | { "type": "keep_sent", item: OutboxId, } | { "type": "list_outbox" } | { "type": "keep_observed", account: string, op: TxLogId | null, } | { "type": "list_rooms" } | { "type": "list_timeline", room: RoomId, page: TimelinePage, limit: number, } | { "type": "fill_gap", room: RoomId, gap: GapId, } | { "type": "list_room_events", room: RoomId, events: Array<RoomEventId>, } | { "type": "list_edits", room: RoomId, event: RoomEventId, } | { "type": "list_matrix_sends", room: RoomId, } | { "type": "send_matrix_message", room: RoomId, content: MatrixContent, } | { "type": "cancel_matrix_send", item: MatrixSendId, } | { "type": "keep_matrix_sent", item: MatrixSendId, } | { "type": "mark_room_read", room: RoomId, event: RoomEventId, } | { "type": "set_typing", room: RoomId, typing: boolean, } | { "type": "list_receipts", room: RoomId, } | { "type": "list_members", room: RoomId, } | { "type": "list_pinned", room: RoomId, } | { "type": "list_thread", room: RoomId, root: RoomEventId, } | { "type": "search_room", room: RoomId, query: string, before: RoomEventId | null, limit: number, } | { "type": "preview_url", room: RoomId, event: RoomEventId, url: string, } | { "type": "create_matrix_room", account: string, room: NewRoom, } | { "type": "start_direct_chat", account: string, user: string, } | { "type": "join_matrix_room", account: string, room: string, } | { "type": "get_matrix_security", account: string, } | { "type": "recover_matrix", account: string, recovery: string, } | { "type": "verify_matrix_device", account: string, device: string | null, } | { "type": "answer_verification", account: string, flow_id: string, answer: VerificationAnswer, } | { "type": "import_room_keys", account: string, blob: BlobId, passphrase: string, } | { "type": "change_membership", room: RoomId, change: MembershipChange, } | { "type": "order_spaces", changes: Array<SpaceOrderChange>, } | { "type": "modify_room_tags", rooms: Array<RoomId>, add: Array<string>, remove: Array<string>, } | { "type": "list_events", from: number, to: number, } | { "type": "list_todos" } | { "type": "create_todo", title: string, notes: string, due: string | null, message: MessageId | null, } | { "type": "edit_todo", todo: TodoId, edit: TodoEdit, } | { "type": "delete_todo", todo: TodoId, };

export type ServerResponse = { "type": "undone", tx_log_id: TxLogId, } | { "type": "dispatched", tx_log_id: TxLogId, } | { "type": "done" } | { "type": "matrix_security", security: MatrixSecurity, } | { "type": "room_keys_imported", imported: number, total: number, } | { "type": "accounts", as_of: TxLogId, accounts: Array<Account>, } | { "type": "conversations", as_of: TxLogId, conversations: Array<Conversation>, 
/**
 * Where the next page starts, if there may be one.
 */
next: ListPosition | null, 
/**
 * The entries the list's conversations are made of, as label
 * changes move entries into and out of them.
 */
scope: ListScope, } | { "type": "entries", as_of: TxLogId, entries: Array<Entry>, } | { "type": "items", as_of: TxLogId, conversations: Array<Conversation>, next: ListPosition | null, scope: ListScope, 
/**
 * Most recently active first.
 */
rooms: Array<Room>, 
/**
 * Open ones first, by due date.
 */
todos: Array<Todo>, 
/**
 * Those yet to end, soonest first, then those past, latest first.
 */
events: Array<CalendarEvent>, } | { "type": "message", message: MessageContent, } | { "type": "drafts", as_of: TxLogId, drafts: Array<Draft>, } | { "type": "outbox", as_of: TxLogId, accounts: Array<AccountOutbox>, } | { "type": "rooms", as_of: TxLogId, accounts: Array<MatrixAccount>, rooms: Array<Room>, } | { "type": "timeline", as_of: TxLogId, room: RoomId, events: Array<RoomEvent>, 
/**
 * Whether there are earlier events than those listed.
 */
more_before: boolean, 
/**
 * The gaps just before the events listed, and after the last, if
 * it is the latest.
 */
gaps: Array<TimelineGap>, } | { "type": "room_events", as_of: TxLogId, room: RoomId, events: Array<RoomEvent>, } | { "type": "edits", as_of: TxLogId, room: RoomId, event: RoomEventId, edits: Array<RoomEvent>, } | { "type": "room_search", as_of: TxLogId, room: RoomId, events: Array<RoomEvent>, more: boolean, } | { "type": "url_preview", preview: UrlPreview | null, } | { "type": "pinned", as_of: TxLogId, room: RoomId, events: Array<RoomEvent>, } | { "type": "thread", as_of: TxLogId, room: RoomId, root: RoomEvent, replies: Array<RoomEvent>, } | { "type": "matrix_sends", as_of: TxLogId, room: RoomId, sends: Array<MatrixSend>, } | { "type": "receipts", room: RoomId, receipts: Array<MemberReceipt>, } | { "type": "matrix_room_joined", room_id: string, } | { "type": "members", room: RoomId, members: Array<MemberInfo>, powers: RoomPowers, } | { "type": "todos", as_of: TxLogId, todos: Array<Todo>, } | { "type": "events", as_of: TxLogId, events: Array<CalendarEvent>, } | { "type": "history", as_of: TxLogId, entries: Array<HistoryEntry>, 
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
 * Which entries a list's conversations are made of (SPEC §4): those
 * with one of the labels, in a list of Trash or Spam; else those with
 * none of them, Trash and Spam (none at all with `in:anywhere`).
 */
export type ListScope = { "type": "within", labels: Array<LabelId>, } | { "type": "except", labels: Array<LabelId>, };

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
export type ServerEvent = { "type": "entries_added" } & EntriesAdded | { "type": "entries_removed" } & EntriesRemoved | { "type": "labels_changed" } & LabelsChanged | { "type": "label_list_changed" } & LabelListChanged | { "type": "draft_edited" } & DraftEdited | { "type": "drafts_changed" } & DraftsChanged | { "type": "outbox_changed" } & OutboxChanged | { "type": "rooms_changed" } & RoomsChanged | { "type": "timeline_added" } & TimelineAdded | { "type": "timeline_filled" } & TimelineFilled | { "type": "timeline_changed" } & TimelineChanged | { "type": "matrix_sends_changed" } & MatrixSendsChanged | { "type": "todos_changed" } & TodosChanged | { "type": "calendar_changed" } & CalendarChanged;

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
 * Spaces being reordered, by the op that asked, newest first.
 */
space_orders: Array<PendingSpaceOrders>, 
/**
 * Rooms being tagged or untagged, by the op that asked, newest
 * first.
 */
room_tags: Array<PendingRoomTags>, 
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
 * Of those, of the main timeline and threads, how many notify, and
 * how many highlight (mention the account, or a keyword), by the
 * account's push rules (SPEC §9.2).
 */
notifying: number, highlighted: number, 
/**
 * The events pinned, in the order pinned (Matrix event ids).
 */
pinned: Array<string>, 
/**
 * Whether the account may pin and unpin events.
 */
may_pin: boolean, 
/**
 * The latest event's time, ms since the Unix epoch.
 */
last_event_ts: number | null, read_up_to: ReadMarker | null, 
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
 * Whether it is a direct chat, by the account's `m.direct` as the
 * room was last synced.
 */
direct: boolean, 
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
children: Array<string>, 
/**
 * `m.room.topic`, if set and not empty.
 */
topic: string | null, 
/**
 * How many members have joined, as the homeserver counts them.
 */
joined_members: number, 
/**
 * Of a space, where the user puts it among the top-level spaces
 * (MSC3230): its `order`, as asked for if being changed.
 */
space_order: string | null, 
/**
 * Its tags (`m.tag`: `m.favourite`, `m.lowpriority`, the user's own
 * `u.*`), as asked for if being changed, in order.
 */
tags: Array<string>, };

export type Encryption = "none" | "decrypted" | "undecryptable";

export type RoomEvent = { id: RoomEventId, 
/**
 * Where it is in the timeline, which is in this order: events
 * fetched later may be earlier (SPEC §9.2).
 */
pos: number, 
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
 * The event as JSON, decrypted if it could be: what clients show of
 * it beyond the fields here (media, state), and its source.
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
replied: RepliedEvent | null, 
/**
 * Of a message with an HTML `formatted_body`, that, sanitized
 * (SPEC §9.2).
 */
html: string | null, 
/**
 * Of the root of a thread, the thread summed up.
 */
thread: ThreadSummary | null, 
/**
 * Of a message of a thread whose root is stored, the root: the
 * thread lists it, not the main timeline.
 */
thread_root: RoomEventId | null, };

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
content: string, 
/**
 * Its HTML `formatted_body`, if it has one, sanitized (SPEC §9.2).
 */
html: string | null, };

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
 * A thread, as its root shows it (SPEC §9.2).
 */
export type ThreadSummary = { 
/**
 * How many messages it has, but the root.
 */
replies: number, latest: RepliedEvent, };

/**
 * What a linked page says of itself (OpenGraph), as the homeserver
 * previews it (SPEC §9.8).
 */
export type UrlPreview = { title: string | null, description: string | null, site_name: string | null, 
/**
 * The page's image, as the homeserver uploaded it: shown by
 * `/api/rooms/{room}/events/{event}/preview/{server}/{media}?url=`.
 */
image: string | null, image_width: number | null, image_height: number | null, };

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
 * What a send sends to a room (SPEC §9.3). Events it is of are named
 * by their Matrix ids, which the homeserver knows them by.
 */
export type MatrixContent = { "type": "text", body: string, markdown: boolean, 
/**
 * `m.emote` (`/me`) rather than `m.text`.
 */
emote: boolean, 
/**
 * The event it replies to.
 */
reply_to: string | null, 
/**
 * The account's own message it edits (`m.replace`).
 */
replaces: string | null, 
/**
 * The root of the thread it is of (`m.thread`); then it replies
 * to the thread's root or one of its messages, if to any.
 */
thread: string | null, } | { "type": "reaction", event: string, key: string, } | { "type": "redaction", event: string, } | { "type": "file", blob: BlobId, name: string, mime: string, reply_to: string | null, } | { "type": "name", name: string, } | { "type": "topic", topic: string, } | { "type": "pin", event: string, pinned: boolean, } | { "type": "member", user: string, action: MemberAction, reason: string | null, };

/**
 * A message being sent, sent and not yet synced back, or sent though
 * cancelled (needing a decision).
 */
export type MatrixSend = { id: MatrixSendId, room: RoomId, content: MatrixContent, state: MatrixSendState, 
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
 * A change to a space's order among the top-level spaces (MSC3230):
 * from `from`, as it is, to `to`; `None` is no order, which puts the
 * space after those with one.
 */
export type SpaceOrderChange = { space: RoomId, from: string | null, to: string | null, };

/**
 * Spaces being reordered by one op.
 */
export type PendingSpaceOrders = { 
/**
 * The op that asked: undoing it puts the spaces back.
 */
op: PendingOp, 
/**
 * The spaces whose order is yet to reach the homeserver.
 */
spaces: Array<RoomId>, 
/**
 * Why the homeserver refused one, if it did: it needs a decision,
 * which undoing the op makes.
 */
refused: string | null, };

/**
 * A room tagged, or untagged.
 */
export type RoomTagChange = { room: RoomId, tag: string, desired: boolean, };

/**
 * Rooms being tagged or untagged by one op.
 */
export type PendingRoomTags = { 
/**
 * The op that asked: undoing it puts the tags back.
 */
op: PendingOp, 
/**
 * The changes yet to reach the homeserver.
 */
changes: Array<RoomTagChange>, 
/**
 * Why the homeserver refused one, if it did: it needs a decision,
 * which undoing the op makes.
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
