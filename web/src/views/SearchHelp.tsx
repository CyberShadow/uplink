import { searchPath } from './App'
import { useTitle } from '../title'
import styles from './SearchHelp.module.css'

/** One way to write a term: an example, and what it lists. */
type Row = [example: string, means: string]

const SECTIONS: { title: string; rows: Row[] }[] = [
  {
    title: 'Words',
    rows: [
      ['invoice', 'Messages with a word starting “invoice”: in the subject, the sender, the recipients or the body'],
      ['invoice march', 'Both words, anywhere in the message'],
      ['"march invoice"', 'The exact phrase'],
      ['invoice OR receipt', 'Either'],
      ['-newsletter', 'Not the word; NOT newsletter is the same'],
      ['(invoice OR receipt) -paid', 'Parentheses group terms'],
    ],
  },
  {
    title: 'People',
    rows: [
      ['from:alice', 'From a sender whose name or address has the word'],
      ['from:"Alice Smith"', 'The exact name'],
      ['to:bob@example.com', 'To or Cc'],
      ['cc:carol', 'Cc'],
      ['from:me', 'Sent from the account’s own address (to:me and cc:me too)'],
    ],
  },
  {
    title: 'Parts of a message',
    rows: [
      ['subject:lunch', 'In the subject'],
      ['body:agenda', 'In the body'],
      ['has:attachment', 'With attachments'],
      ['larger:5M', 'Bigger than 5 MB (K, M and G count in 1024s; smaller: too)'],
      ['id:<1234@example.com>', 'By its Message-ID (rfc822msgid: too)'],
    ],
  },
  {
    title: 'Labels, folders and accounts',
    rows: [
      ['label:Projects', 'With the label or folder, in any account (tag: too)'],
      ['label:"Project X"', 'A name with spaces, quoted'],
      ['in:inbox', 'In the Inbox; in:sent, in:drafts, in:trash, in:spam, or any label'],
      ['in:anywhere', 'Trash and Spam too'],
      ['is:unread', 'Unread; is:read, is:starred, is:important'],
      ['category:promotions', 'In a Gmail category'],
      ['account:work', 'Of the account, by its id or address'],
    ],
  },
  {
    title: 'Rooms, to-dos and events',
    rows: [
      ['is:room', 'Rooms, and their messages; is:mail, is:todo and is:event too'],
      ['is:unread is:room', 'Rooms with messages unread, and those messages'],
      ['is:new', 'Messages not yet caught up on, however far they were read'],
      ['is:mention', 'Messages that mention you, read or not: is:mention is:new, those new'],
      ['is:notifying', 'Messages that notify, by your rules (none of muted rooms)'],
      ['is:dm', 'Direct chats'],
      ['is:invite', 'Rooms you are invited to'],
      ['tag:lowpriority', 'Rooms with the tag: favourite, lowpriority, or one of yours'],
      ['space:Work', 'Rooms in the space, however deeply, and their messages'],
      ['from:alice is:room', 'Messages in rooms by a sender whose name or id has the word'],
      ['due:today', 'To-dos due today; due:tomorrow, due:2026-12-24'],
      ['is:overdue', 'Open to-dos due before today'],
      ['has:due', 'To-dos with a due date'],
      ['is:done', 'Done to-dos, which are otherwise left out'],
    ],
  },
  {
    title: 'Dates',
    rows: [
      ['after:2026-01-31', 'Received on or after the day (2026/1/31 too)'],
      ['before:2026-02-01', 'Received before the day'],
      ['newer_than:2w', 'In the last two weeks: d, w, m (30 days) or y (365)'],
      ['older_than:1y', 'More than a year ago'],
    ],
  },
]

/** The search language (SPEC §8.7), with examples to try. */
export function SearchHelp() {
  useTitle('Search help')
  return (
    <article class={styles.help} aria-label="Search help">
      <h2>Searching</h2>
      <p>
        A search lists mail, rooms, their messages, to-dos and calendar events. Every term must match; a term of one
        kind matches nothing of another (<code>from:</code> no to-do, <code>is:overdue</code> no mail), and so its
        negation everything of it. A conversation is listed when one of its messages matches. Mail in Trash and Spam
        is left out, unless the search names <code>in:trash</code>, <code>in:spam</code> or <code>in:anywhere</code>,
        which searches Trash or Spam instead, or everything. Words match a room's name and its messages' text (the
        latest 50 of them; a room's own search finds more), a to-do's title and notes, and an event's subject,
        location and preview. Dates are of a room's latest message, a to-do's creation and an event's start.
      </p>
      {SECTIONS.map((section) => (
        <section key={section.title}>
          <h3>{section.title}</h3>
          <table>
            <tbody>
              {section.rows.map(([example, means]) => (
                <tr key={example}>
                  <td>
                    <a href={searchPath(example)}>
                      <code>{example}</code>
                    </a>
                  </td>
                  <td>{means}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      ))}
      <p>
        Operators, words and label names are not case-sensitive. An operator uplink does not know is an error
        rather than text, as is a label that no account and no room has; quote it, as in <code>"re:lunch"</code>, to search for
        the text. Dates are of UTC.
      </p>
    </article>
  )
}
