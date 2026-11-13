//! The wire types as TypeScript, which the web client imports, and the
//! protocol's version, of them.

use crate::{calendar::*, compose::*, mail::*, matrix::*, todos::*, *};
use ts_rs::{Config, TS};

/// The wire types as TypeScript, for `web/src/generated/protocol.ts`, and
/// their [`version`].
pub fn typescript() -> String {
    let declarations = declarations();
    format!(
        "{declarations}\n/** Of these types: what `Hello` says the client speaks. */\nexport const PROTOCOL_VERSION = \"{}\"\n",
        version_of(&declarations)
    )
}

/// What the client and the server must agree on to understand each
/// other: a hash of the wire types' declarations. A change to any of
/// them changes it.
pub fn version() -> &'static str {
    static VERSION: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| version_of(&declarations()));
    &VERSION
}

/// FNV-1a, 64 bits: stable across builds and platforms, as `std`'s
/// hashers are not promised to be.
fn version_of(declarations: &str) -> String {
    let hash = declarations
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

fn declarations() -> String {
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
        RoomPreview,
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
        HomeBucket,
        Badge,
        Badges,
        Marks,
        Place,
        Mark,
        BadgedBucket,
        Activity,
        FoundMessage,
        ErrorCode,
        LogEntry,
        ListScope,
        ConversationSet,
        ConversationAction,
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
        RemoteHost,
        ImageRule,
        ImageRuleChanged,
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
        SpaceOrderChange,
        PendingSpaceOrders,
        RoomTagChange,
        PendingRoomTags,
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
        ShowAs,
        EventResponse,
        EventDetails,
        Person,
        Attendee,
        AttendeeKind,
        CalendarChanged,
    );
    out
}
