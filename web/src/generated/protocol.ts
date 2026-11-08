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

export type ClientRequest = { "type": "undo", tx_log_id: TxLogId, } | { "type": "list_accounts" } | { "type": "list_entries", label: LabelId, after: ListPosition | null, limit: number, } | { "type": "search", query: string, limit: number, } | { "type": "load_message", message: MessageId, } | { "type": "modify_labels", entries: Array<EntryId>, add: Array<LabelId>, remove: Array<LabelId>, };

export type ServerResponse = { "type": "undone", tx_log_id: TxLogId, } | { "type": "dispatched", tx_log_id: TxLogId, } | { "type": "accounts", as_of: TxLogId, accounts: Array<Account>, } | { "type": "entries", as_of: TxLogId, entries: Array<Entry>, 
/**
 * Where the next page starts, if there may be one.
 */
next: ListPosition | null, } | { "type": "message", message: MessageContent, };

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

export type Source = { "type": "user" } | { "type": "account", id: string, };

/**
 * A change to state that clients mirror (SPEC §5.4). Events carry ids
 * rather than content, since they are stored in the log; clients query
 * what they display.
 */
export type ServerEvent = { "type": "entries_added" } & EntriesAdded | { "type": "entries_removed" } & EntriesRemoved | { "type": "labels_changed" } & LabelsChanged | { "type": "label_list_changed" } & LabelListChanged;

export type MessageId = number;

export type EntryId = number;

export type LabelId = number;

export type Account = { id: string, address: string, 
/**
 * Whether the first full sync has completed.
 */
synced: boolean, labels: Array<Label>, };

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
date_sent: number | null, body_text: string, };

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
