import { beforeEach, describe, expect, it, vi } from 'vitest'
import { FakeServer, ME } from '../test/server'
import { Client } from './client'
import { TIMELINE_PAGE } from './matrix'
import { sendSummary, text } from '../matrix'

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

  it('loads again the events whose edits, reactions or redaction changed', async () => {
    const room = server.addRoom('Lounge')
    const first = server.receive(room.id, { body: 'first' })
    server.receive(room.id, { body: 'second' })
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['first', 'second']))
    server.change(room.id, first, { redacted: true, body: null })
    await vi.waitFor(() => expect(bodies(room.id)).toEqual([null, 'second']))
    expect(server.requests.at(-1)).toEqual({ type: 'list_room_events', room: room.id, events: [first.id] })
  })

  it('gives a thread the messages that turn out to be of it, and loads it again as it changes', async () => {
    const room = server.addRoom('Lounge')
    const root = server.receive(room.id, { body: 'root' })
    // Listed in the main timeline while its root was not stored.
    const early = server.receive(room.id, { body: 'early' })
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['root', 'early']))
    server.change(room.id, early, { thread_root: root.id })
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['root']))

    const thread = client.matrix.thread(room.id, root.id)
    await vi.waitFor(() => expect(thread.state.value?.replies.map((e) => e.body)).toEqual(['early']))
    server.receiveInThread(room.id, root, { body: 'later' })
    await vi.waitFor(() => expect(thread.state.value?.replies.map((e) => e.body)).toEqual(['early', 'later']))
    await vi.waitFor(() => expect(client.matrix.timeline(room.id).events.value?.[0].thread?.replies).toBe(2))
    expect(bodies(room.id)).toEqual(['root'])
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

  it('lists the messages being sent with the timeline, undoably until sent', async () => {
    const room = server.addRoom('Lounge')
    client.reset()
    const sends = () => client.matrix.timeline(room.id).sends.value
    await vi.waitFor(() => expect(sends()).toEqual([]))
    const sent = await client.matrix.send(room.id, text('hi there'))
    await vi.waitFor(() => expect(sends()?.map((i) => sendSummary(i.content))).toEqual(['hi there']))
    await client.ops.undo(sent!.id)
    await vi.waitFor(() => expect(sends()).toEqual([]))

    await client.matrix.send(room.id, text('again'))
    server.echo(server.matrixSends[0].id)
    await vi.waitFor(() => expect(sends()).toEqual([]))
    await vi.waitFor(() => expect(bodies(room.id)).toEqual(['again']))
    expect(client.matrix.timeline(room.id).events.value![0].sender).toBe(ME)
  })
})
