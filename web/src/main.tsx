import { render } from 'preact'
import { Connection } from './connection'
import { Client } from './stores/client'
import { App } from './views/App'
import './styles.css'

const url = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
const client = new Client({ request: (body) => connection.request(body) })
const connection = new Connection(url, {
  reset: () => client.reset(),
  entry: (entry) => client.apply(entry),
})

render(<App client={client} status={connection.status} />, document.getElementById('app')!)
