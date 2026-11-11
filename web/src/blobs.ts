// Files uploaded to the server's blob store, to attach to drafts (SPEC
// §7.4). Over HTTP rather than the WebSocket: they are large, and binary.

import type { MessageId, UploadedBlob } from './generated/protocol'

export interface Blobs {
  upload(file: Blob): Promise<UploadedBlob>
  /** Stores attachment `index` of `message` as a blob, to forward it. */
  copyAttachment(message: MessageId, index: number): Promise<UploadedBlob>
  /** The Matrix account's room keys, exported encrypted with `passphrase`
   * (SPEC §9.4). */
  exportRoomKeys(account: string, passphrase: string): Promise<Blob>
}

export const httpBlobs: Blobs = {
  upload: (file) => post('/api/blobs', file),
  copyAttachment: (message, index) => post(`/api/messages/${message}/attachments/${index}/blob`, null),
  exportRoomKeys: async (account, passphrase) => {
    const response = await fetch(`/api/matrix/${encodeURIComponent(account)}/room-keys`, {
      method: 'POST',
      body: passphrase,
    })
    if (!response.ok) throw new Error(`Exporting the room keys failed: ${await response.text()}`)
    return await response.blob()
  },
}

async function post(path: string, body: Blob | null): Promise<UploadedBlob> {
  const response = await fetch(path, { method: 'POST', body })
  if (response.status === 413) throw new Error('The file is too large to attach')
  if (!response.ok) throw new Error(`Uploading failed: ${await response.text()}`)
  return (await response.json()) as UploadedBlob
}
