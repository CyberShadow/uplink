//! Prints the wire types as TypeScript, for `web/src/generated/protocol.ts`:
//!
//!     cargo run -q -p uplink-protocol --example typescript > web/src/generated/protocol.ts
//!
//! The `protocol-ts` flake check fails if the checked-in file is stale.

use ts_rs::{Config, TS};
use uplink_protocol::{compose::*, mail::*, matrix::*, todos::*, *};

fn main() {
    // Ids and timestamps are i64, well within a double's exact range.
    let cfg = Config::new().with_large_int("number");
    let mut out = String::from(
        "// Generated from crates/uplink-protocol by its `typescript` example; do not edit.\n",
    );
    macro_rules! export {
        ($($t:ty),* $(,)?) => {$(
            out.push('\n');
            if let Some(docs) = <$t>::docs() {
                out.push_str(&docs);
            }
            out.push_str("export ");
            out.push_str(&<$t>::decl(&cfg));
            out.push('\n');
        )*};
    }
    export!(
        TxLogId,
        WsClientMsg,
        WsServerMsg,
        ClientRequest,
        ServerResponse,
        ServerError,
        ErrorCode,
        LogEntry,
        EntryList,
        HistoryEntry,
        Source,
        ServerEvent,
        MessageId,
        EntryId,
        LabelId,
        Account,
        Label,
        Entry,
        ListPosition,
        MessageContent,
        EntriesAdded,
        EntriesRemoved,
        LabelsChanged,
        LabelState,
        LabelListChanged,
        DraftId,
        OutboxId,
        DraftContent,
        Draft,
        DraftField,
        Splice,
        OutboxState,
        OutboxItem,
        DraftEdited,
        DraftsChanged,
        OutboxChanged,
        RoomId,
        RoomEventId,
        MatrixOutboxId,
        MatrixAccount,
        Membership,
        Room,
        Encryption,
        RoomEvent,
        TimelinePage,
        MatrixOutboxItem,
        RoomsChanged,
        TimelineAdded,
        MatrixOutboxChanged,
        TodoId,
        Todo,
        TodoEdit,
        TodosChanged,
    );
    print!("{out}");
}
