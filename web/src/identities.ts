// A mail account's identities (SPEC §3.2): the addresses it sends from,
// which are the user's in its mail. As the server's
// (crates/uplink-email/src/identity.rs).

import type { Identity } from './generated/protocol'

/** The identity `addr` is of: the one of that address, else of its
 * domain. Addresses are compared without regard to case. */
export function identityOf(identities: readonly Identity[], addr: string): Identity | undefined {
  const lower = addr.toLowerCase()
  const domain = lower.slice(lower.lastIndexOf('@') + 1)
  return (
    identities.find((i) => 'address' in i && i.address.toLowerCase() === lower) ??
    (lower.includes('@') ? identities.find((i) => 'domain' in i && i.domain.toLowerCase() === domain) : undefined)
  )
}

/** Whether `addr` is the user's in the account of `identities`. */
export function isOwn(identities: readonly Identity[], addr: string): boolean {
  return identityOf(identities, addr) !== undefined
}

/** The account's own address, which mail is sent from unless a draft
 * names another. */
export function defaultAddress(identities: readonly Identity[]): string {
  const first = identities[0]
  if (!first || !('address' in first)) throw new Error('a mail account’s identities begin with its address')
  return first.address
}
