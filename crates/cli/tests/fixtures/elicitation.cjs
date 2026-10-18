// Offline MCP server: a tool call only completes after a client elicitation reply.
const fs = require('fs');
const send = value => process.stdout.write(JSON.stringify({jsonrpc: '2.0', ...value}) + '\n');
let pending;
require('readline').createInterface({input: process.stdin}).on('line', line => {
  const request = JSON.parse(line);
  if (request.method === undefined) {
    if (request.id !== 'server-elicitation') throw Error('Wrong elicitation response ID');
    fs.writeFileSync(process.env.ELICITATION_RESULT, JSON.stringify(request));
    send({id: pending, result: {content: [{type: 'text', text: JSON.stringify(request)}]}});
    return;
  }
  if (request.id === undefined) return;
  let result = {};
  switch (request.method) {
    case 'initialize':
      if (!request.params.capabilities.elicitation) throw Error('Missing elicitation capability');
      result = {protocolVersion: '2025-06-18', capabilities: {tools: {}}, serverInfo: {name: 'fixture', version: '1'}};
      break;
    case 'tools/list':
      result = {tools: [{name: 'elicit', description: 'Ask for a number', inputSchema: {type: 'object'}}]};
      break;
    case 'resources/list': result = {resources: []}; break;
    case 'tools/call':
      pending = request.id;
      send({method: 'notifications/message', params: {level: 'info', data: 'before request'}});
      send({id: 'server-elicitation', method: 'elicitation/create', params: {
        message: 'Choose a number', requestedSchema: {
          type: 'object', properties: {number: {type: 'integer', minimum: 1}}, required: ['number'], additionalProperties: false
        }
      }});
      return;
  }
  send({id: request.id, result});
});
