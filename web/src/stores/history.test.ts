import { beforeEach, describe, expect, it, vi } from 'vitest'
import { FakeServer } from '../test/server'
import { Client } from './client'
import { HISTORY_PAGE } from './history'

let server: FakeServer
let client: Client

beforeEach(() => {
  client = new Client((server = new FakeServer()))
  server.attach(client)
})

const listed = () =>
  client.history.entries.value?.map((h) => [h.entry.description, h.undone_by !== null] as const)

describe('HistoryStore', () => {
  it('lists the user’s ops newest first, then applies later ones and undos', async () => {
    await client.todos.create('One')
    await client.todos.create('Two')
    client.reset()
    await vi.waitFor(() => expect(listed()).toEqual([
      ["Added 'Two'", false],
      ["Added 'One'", false],
    ]))

    const undo = await client.ops.undo(client.history.entries.value![1].entry.id)
    expect(undo).not.toBeNull()
    expect(listed()).toEqual([
      ['Undid it', false],
      ["Added 'Two'", false],
      ["Added 'One'", true],
    ])
    expect(client.history.entries.value![0].entry.undoes).toBe(client.history.entries.value![2].entry.id)
  })

  it('pages back through older ones', async () => {
    for (let i = 0; i < HISTORY_PAGE + 3; i++) await client.todos.create(`t${i}`)
    client.reset()
    await vi.waitFor(() => expect(client.history.entries.value).toHaveLength(HISTORY_PAGE))
    expect(client.history.more.value).toBe(true)
    client.history.loadMore()
    await vi.waitFor(() => expect(client.history.entries.value).toHaveLength(HISTORY_PAGE + 3))
    expect(client.history.more.value).toBe(false)
    expect(listed()!.at(-1)).toEqual(["Added 't0'", false])
  })

  it('undoes the last op, then the ones before it, until something else is done', async () => {
    for (const title of ['One', 'Two', 'Three']) await client.todos.create(title)
    client.reset()
    await vi.waitFor(() => expect(listed()).toHaveLength(3))
    const titles = () => client.todos.todos.value!.map((t) => t.title).sort()

    // Pressed in quick succession, each goes on from the last.
    client.history.undoLast()
    await client.history.undoLast()
    expect(listed()).toEqual([
      ['Undid it', false],
      ['Undid it', false],
      ["Added 'Three'", true],
      ["Added 'Two'", true],
      ["Added 'One'", false],
    ])

    // Something done since: it is the last op.
    await client.todos.create('Four')
    await client.history.undoLast()
    await client.history.undoLast()
    expect(listed()!.filter(([, undone]) => !undone).map(([d]) => d)).toEqual([
      'Undid it',
      'Undid it',
      'Undid it',
      'Undid it',
    ])
    await vi.waitFor(() => expect(titles()).toEqual([]))

    await client.history.undoLast()
    expect(client.ops.notice.value).toEqual({ kind: 'failed', message: 'Nothing more to undo' })
  })
})
