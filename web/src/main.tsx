import { render } from 'preact'
import { httpBlobs } from './blobs'
import { Connection } from './connection'
import { Client } from './stores/client'
import { App } from './views/App'
import './styles.css'

const url = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
const client = new Client({ request: (body) => connection.request(body), ...httpBlobs })
const connection = new Connection(url, {
  reset: () => client.reset(),
  entry: (entry) => client.apply(entry),
  notice: (notice) => client.notice(notice),
})

render(<App client={client} status={connection.status} />, document.getElementById('app')!)
