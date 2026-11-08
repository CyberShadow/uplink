import { render } from 'preact'
import { Connection } from './connection'
import { MailStore } from './stores/mail'
import { App } from './views/App'
import './styles.css'

const url = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
const mail = new MailStore({ request: (body) => connection.request(body) })
const connection = new Connection(url, {
  reset: () => mail.reset(),
  entry: (entry) => mail.apply(entry),
})

render(<App mail={mail} status={connection.status} />, document.getElementById('app')!)
