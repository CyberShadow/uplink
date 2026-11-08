import { beforeEach, describe, expect, it, vi } from 'vitest'
import { FakeServer, ME } from '../test/server'
import { Client } from './client'
import { TIMELINE_PAGE } from './matrix'

let server: FakeServer
let client: Client

beforeEach(() => {
  server = new FakeServer()
  server.addMatrixAccount()
  client = new Client(server)
  server.attach(client)
})

const bodies = (room: number) => client.matrix.timeline(room).events.value?.map((e) => e.body)

describe('MatrixStore', () => {
  it('lists rooms with their unread counts, as they change', async () => {
    const room = server.addRoom('Lounge')
    client.reset()
    await vi.waitFor(() => expect(client.matrix.rooms.value?.map((r) => r.name)).toEqual(['Lounge']))
    server.receive(room.id, { body: 'hello' })
    await vi.waitFor(() => expect(client.matrix.rooms.value?.[0].unread).toBe(1))
  })

  it('loads the latest events, then those synced later, then earlier ones', async () => {
    const room = server.addRoom('Lounge')
    for (let i = 0; i < TIMELINE_PAGE + 5; i++) server.receive(room.id, { body: `m${i}` })
    const timeline = client.matrix.timeline(room.id)
    await vi.waitFor(() => expect(bodies(room.id)).toHaveLength(TIMELINE_PAGE))
    expect(bodies(room.id)![0]).toBe('m5')
    expect(timeline.moreBefore.value).toBe(true)

    server.receive(room.id, { body: 'later' })
    await vi.waitFor(() => expect(bodies(room.id)!.at(-1)).toBe('later'))

    await timeline.loadEarlier()
    expect(bodies(room.id)![0]).toBe('m0')
    expect(bodies(room.id)).toHaveLength(TIMELINE_PAGE + 6)
    expect(timeline.moreBefore.value).toBe(false)
  })

  it('loads every page synced while it was away', async () => {
    const room = server.addRoom('Lounge')
    server.receive(room.id, { body: 'first' })
    const timeline = client.matrix.timeline(room.id)
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['first']))
    // Many at once, as after a reconnection.
    server.meanwhile = () => {
      for (let i = 0; i < TIMELINE_PAGE * 2; i++) server.receive(room.id, { body: `m${i}` })
    }
    timeline.added()
    await vi.waitFor(() => expect(bodies(room.id)).toHaveLength(TIMELINE_PAGE * 2 + 1))
  })

  it('reloads the timelines on show afresh after a reset', async () => {
    const room = server.addRoom('Lounge')
    server.receive(room.id, { body: 'first' })
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['first']))
    client.reset()
    expect(bodies(room.id)).toBeUndefined()
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['first']))
  })

  it('sends through the outbox, undoably until sent', async () => {
    const room = server.addRoom('Lounge')
    client.reset()
    await vi.waitFor(() => expect(client.matrix.outbox.value).toEqual([]))
    const sent = await client.matrix.send(room.id, 'hi there')
    await vi.waitFor(() => expect(client.matrix.outbox.value?.map((i) => i.body)).toEqual(['hi there']))
    await client.ops.undo(sent!.id)
    await vi.waitFor(() => expect(client.matrix.outbox.value).toEqual([]))

    await client.matrix.send(room.id, 'again')
    server.echo(server.matrixOutbox[0].id)
    await vi.waitFor(() => expect(client.matrix.outbox.value).toEqual([]))
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['again']))
    expect(client.matrix.timeline(room.id).events.value![0].sender).toBe(ME)
  })
})
