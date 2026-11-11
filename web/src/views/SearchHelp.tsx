import { searchPath } from './App'
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
  return (
    <article class={styles.help} aria-label="Search help">
      <h2>Searching mail</h2>
      <p>
        Every term must match; a conversation is listed when one of its messages does. Mail in Trash and Spam is left
        out, unless the search names <code>in:trash</code>, <code>in:spam</code> or <code>in:anywhere</code>, which
        searches Trash or Spam instead, or everything.
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
        rather than text, as is a label that no account has; quote it, as in <code>"re:lunch"</code>, to search for
        the text. Dates are of UTC.
      </p>
    </article>
  )
}
