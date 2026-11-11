//! Prints the wire types as TypeScript, for `web/src/generated/protocol.ts`:
//!
//!     cargo run -q -p uplink-protocol --example typescript > web/src/generated/protocol.ts
//!
//! The `protocol-ts` flake check fails if the checked-in file is stale.

use ts_rs::{Config, TS};
use uplink_protocol::{calendar::*, compose::*, mail::*, matrix::*, todos::*, *};

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
        Notice,
        Typing,
        RoomMember,
        ReceiptsChanged,
        MemberReceipt,
        MemberInfo,
        NewRoom,
        MemberState,
        RoomPowers,
        MemberAction,
        SecurityChanged,
        MatrixSecurity,
        KeyBackup,
        Recovery,
        MatrixDevice,
        ReadMarker,
        GapId,
        TimelineGap,
        TimelineFilled,
        MatrixVerification,
        VerificationPhase,
        SasEmoji,
        VerificationAnswer,
        ClientRequest,
        ServerResponse,
        ServerError,
        ErrorCode,
        LogEntry,
        ListScope,
        HistoryEntry,
        Source,
        ServerEvent,
        MessageId,
        EntryId,
        LabelId,
        Account,
        MailProvider,
        Label,
        Entry,
        Conversation,
        ListingProgress,
        Member,
        Sender,
        ListPosition,
        MessageContent,
        Attachment,
        EntriesAdded,
        EntriesRemoved,
        LabelsChanged,
        LabelState,
        LabelListChanged,
        DraftId,
        OutboxId,
        DraftContent,
        BlobId,
        UploadedBlob,
        DraftAttachmentId,
        DraftAttachment,
        Draft,
        DraftField,
        Splice,
        SendState,
        OutboxItem,
        AccountOutbox,
        Pending,
        Failing,
        PendingChanges,
        PendingOp,
        Refusal,
        DraftEdited,
        DraftsChanged,
        OutboxChanged,
        RoomId,
        RoomEventId,
        MatrixSendId,
        MatrixAccount,
        Membership,
        MembershipChange,
        Room,
        Encryption,
        RoomEvent,
        EventEdit,
        Reaction,
        Reactor,
        RepliedEvent,
        ThreadSummary,
        UrlPreview,
        TimelinePage,
        MatrixSendState,
        MatrixContent,
        MatrixSend,
        PendingMembership,
        RoomsChanged,
        TimelineAdded,
        TimelineChanged,
        MatrixSendsChanged,
        TodoId,
        Todo,
        TodoEdit,
        TodosChanged,
        EventId,
        CalendarEvent,
        CalendarChanged,
    );
    print!("{out}");
}
