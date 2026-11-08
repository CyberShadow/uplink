// Text edits as splices (SPEC §7.2): replace `delete` at `offset` with
// `insert`, offsets in UTF-16 code units, as JavaScript strings count.

export interface Edit {
  offset: number
  delete: string
  insert: string
}

function isHighSurrogate(code: number): boolean {
  return code >= 0xd800 && code <= 0xdbff
}

function isLowSurrogate(code: number): boolean {
  return code >= 0xdc00 && code <= 0xdfff
}

/** The edit turning `from` into `to`: the span between their common
 * prefix and suffix, never splitting a surrogate pair. `null` if equal. */
export function diff(from: string, to: string): Edit | null {
  if (from === to) return null
  let start = 0
  const shorter = Math.min(from.length, to.length)
  while (start < shorter && from.charCodeAt(start) === to.charCodeAt(start)) start++
  if (start > 0 && isHighSurrogate(from.charCodeAt(start - 1))) start--
  let end = 0
  while (
    end < shorter - start &&
    from.charCodeAt(from.length - 1 - end) === to.charCodeAt(to.length - 1 - end)
  ) {
    end++
  }
  if (end > 0 && isLowSurrogate(from.charCodeAt(from.length - end))) end--
  return {
    offset: start,
    delete: from.slice(start, from.length - end),
    insert: to.slice(start, to.length - end),
  }
}

/** `text` with `edit` applied; throws if what it deletes is not there. */
export function apply(text: string, edit: Edit): string {
  const end = edit.offset + edit.delete.length
  if (text.slice(edit.offset, end) !== edit.delete) {
    throw new Error(`edit at ${edit.offset} does not match the text`)
  }
  return text.slice(0, edit.offset) + edit.insert + text.slice(end)
}

/** `local`, an edit of some text, moved past `remote`, a concurrent edit
 * of the same text, so that it applies to the text `remote` produced.
 * Where they overlap, the local edit gives up deleting what the remote one
 * changed: neither side's insertion is lost. */
export function transform(local: Edit, remote: Edit): Edit {
  const localEnd = local.offset + local.delete.length
  const remoteEnd = remote.offset + remote.delete.length
  const shift = remote.insert.length - remote.delete.length
  if (localEnd <= remote.offset) {
    // Entirely before; of two insertions at one point, the local one goes
    // first.
    return local
  }
  if (local.offset >= remoteEnd) {
    return { ...local, offset: local.offset + shift }
  }
  if (local.offset < remote.offset) {
    // Overlapping, starting before: keep the part before the remote edit.
    return {
      offset: local.offset,
      delete: local.delete.slice(0, remote.offset - local.offset),
      insert: local.insert,
    }
  }
  // Overlapping, starting within: keep the part after the remote edit,
  // which now starts after its insertion.
  return {
    offset: remote.offset + remote.insert.length,
    delete: localEnd > remoteEnd ? local.delete.slice(remoteEnd - local.offset) : '',
    insert: local.insert,
  }
}

/** `local` rebased from `base` onto `server`: the local changes to `base`,
 * applied to `server`. */
export function rebase(base: string, local: string, server: string): string {
  const mine = diff(base, local)
  const theirs = diff(base, server)
  if (!mine) return server
  if (!theirs) return local
  return apply(server, transform(mine, theirs))
}

/** Where `position` in a text moves to when `edit` is applied to it. */
export function movePosition(position: number, edit: Edit): number {
  if (position <= edit.offset) return position
  const end = edit.offset + edit.delete.length
  if (position >= end) return position + edit.insert.length - edit.delete.length
  return edit.offset + edit.insert.length
}
