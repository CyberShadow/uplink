import { describe, expect, it } from 'vitest'
import { QUOTE_CLASS, foldHtml, forwards, splitText } from './quote'

/** What `foldHtml` leaves showing, and what it folds, as text. */
function folded(html: string): [string, string] | null {
  const [result, found] = foldHtml(html)
  if (!found) return null
  const template = document.createElement('template')
  template.innerHTML = result
  const quote = template.content.querySelector(`.${QUOTE_CLASS}`)!
  quote.remove()
  return [template.content.textContent!.trim(), quote.textContent!.trim()]
}

describe('splitText', () => {
  it('splits off a trailing quote and its attribution', () => {
    const text = 'Sounds good.\n\nOn Mon, 9 Nov 2026, Alice <a@x.org> wrote:\n> Lunch?\n>\n> A\n'
    expect(splitText(text)).toEqual(['Sounds good.', 'On Mon, 9 Nov 2026, Alice <a@x.org> wrote:\n> Lunch?\n>\n> A\n'])
  })

  it('takes an attribution wrapped over two lines', () => {
    const text = 'Yes.\n\nOn Mon, 9 Nov 2026 at 10:00, Alice Example\n<a@x.org> wrote:\n> Lunch?'
    expect(splitText(text)[1]).toBe('On Mon, 9 Nov 2026 at 10:00, Alice Example\n<a@x.org> wrote:\n> Lunch?')
  })

  it('keeps a quote answered in between, and folds one that is all there is', () => {
    const inline = '> Lunch?\nYes.\n> Where?\nThe usual.'
    expect(splitText(inline)).toEqual([inline, null])
    expect(splitText('\nOn Mon, Alice wrote:\n> Lunch?')).toEqual(['', 'On Mon, Alice wrote:\n> Lunch?'])
    expect(splitText('No quote here.')).toEqual(['No quote here.', null])
  })

  it("folds from Outlook's separator on", () => {
    const text = 'Done.\n\n-----Original Message-----\nFrom: Alice\nSubject: Task\n\nPlease do it.'
    expect(splitText(text)).toEqual(['Done.', '-----Original Message-----\nFrom: Alice\nSubject: Task\n\nPlease do it.'])
  })
})

describe('foldHtml', () => {
  it("folds Gmail's quote", () => {
    expect(
      folded(
        '<div dir="ltr">Sounds good.</div><br><div class="gmail_quote"><div class="gmail_attr">On Mon, Alice wrote:<br></div><blockquote class="gmail_quote">Lunch?</blockquote></div>',
      ),
    ).toEqual(['Sounds good.', 'On Mon, Alice wrote:Lunch?'])
  })

  it('folds a cited quote with the attribution before it, as uplink and Apple Mail write them', () => {
    expect(folded('<p>Yes.</p><p>On 9 Nov, Alice wrote:</p><blockquote><p>Lunch?</p></blockquote>')).toEqual([
      'Yes.',
      'On 9 Nov, Alice wrote:Lunch?',
    ])
    expect(
      folded('<div>Yes.</div><div><br><blockquote type="cite"><div>Lunch?</div></blockquote></div>'),
    ).toEqual(['Yes.', 'Lunch?'])
  })

  it("folds from Outlook's header of the original on, in and out of its containers", () => {
    expect(
      folded(
        '<div class="WordSection1"><p>Done.</p><div style="border:none;border-top:solid #E1E1E1 1.0pt"><p><b>From:</b> Alice</p></div><p>Please do it.</p></div>',
      ),
    ).toEqual(['Done.', 'From: AlicePlease do it.'])
    expect(
      folded('<div>Done.</div><hr style="display:inline-block"><div id="divRplyFwdMsg"><b>From:</b> Alice</div><div>Please do it.</div>'),
    ).toEqual(['Done.', 'From: AlicePlease do it.'])
  })

  it('keeps quotes answered in between', () => {
    expect(folded('<blockquote>Lunch?</blockquote><p>Yes.</p><blockquote>Where?</blockquote><p>There.</p>')).toBeNull()
    expect(folded('<p>Nothing quoted.</p>')).toBeNull()
  })

  it('folds a message of nothing but quote whole, as one sent for its attachment', () => {
    expect(
      folded('<br><div class="gmail_quote">On Mon, Alice wrote:<blockquote>Lunch?</blockquote></div>'),
    ).toEqual(['', 'On Mon, Alice wrote:Lunch?'])
  })
})

describe('forwards', () => {
  it('knows forwards by their subject', () => {
    expect(forwards('Fwd: Report')).toBe(true)
    expect(forwards('FW: Report')).toBe(true)
    expect(forwards('Re: Fwd: Report')).toBe(false)
    expect(forwards(null)).toBe(false)
  })
})
