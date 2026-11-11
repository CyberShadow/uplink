# uplink - personal communication and productivity tool

Web-based single-user email / Matrix / calendar client, and task management.

Goals:

- Data is stored on the server (slim client state).
- Real-time updates and sync (including client state between multiple client instances, such as message drafts).
- Maximally leverage ecosystem of existing libraries (esp. email/Matrix), both frontend and backend.
- Usable as drop-in replacement for Gmail (thus should be able to fetch mail from Gmail).
- Usable as drop-in replacement for Outlook / Outlook Web (thus should be able to fetch mail and calendar from Microsoft 365).
- UI oriented towards a single power user, focusing on productivity and responsiveness, not user-friendliness. Adopt Gmail's keyboard shortcuts scheme.
- Support multiple mailbox and Matrix accounts.
- Transaction log + undo support (undo tagging/deletion/any other mass operation).
- Android app (which may wrap a web view, but needs to have tight platform integration such as low-latency notifications).

Non-goals:

- Multi-tenancy.
- Offline mode.
- End-to-end encryption. (Server is trusted.)
- Chat protocol support besides Matrix. Matrix bridges can be used for that instead.
