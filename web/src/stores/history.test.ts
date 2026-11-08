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
})
