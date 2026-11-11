// Queries, and keeping their results current as log entries invalidate
// them.

import type { Requester } from '../connection'
import type { ClientRequest, ServerResponse } from '../generated/protocol'

export type Response<T extends ServerResponse['type']> = Extract<ServerResponse, { type: T }>

/** Sends `request`, which is answered with a response of `type`. */
export async function query<T extends ServerResponse['type']>(
  server: Requester,
  request: ClientRequest,
  type: T,
): Promise<Response<T>> {
  const response = await server.request(request)
  if (response.type !== type) {
    throw new Error(`expected ${type} in response to ${request.type}, got ${response.type}`)
  }
  return response as Response<T>
}

/** A query's state: whether one is in flight, and whether the state it
 * loads went stale meanwhile (SPEC §12.6). */
export class Loader {
  private loading = false
  private stale = false

  /** `logged`: whether the state is the log's, its responses positioned
   * in it; else (as receipts) a query in flight may not reflect a change,
   * and one follows it. */
  constructor(
    private readonly load: () => Promise<void>,
    private readonly logged = true,
  ) {}

  invalidate() {
    // A query in flight reflects every entry received before its response.
    if (this.loading && this.logged) return
    this.stale = true
    this.pump()
  }

  private async pump() {
    if (this.loading || !this.stale) return
    this.stale = false
    this.loading = true
    try {
      await this.load()
    } finally {
      this.loading = false
    }
    this.pump()
  }
}
