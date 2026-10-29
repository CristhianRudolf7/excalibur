import json, sys
mode = sys.argv[1] if len(sys.argv)>1 else 'pull'
docs = {}
pending_rename=None
metrics = {'read_probes':{},'opens':0,'changes':0,'cancelled':0,'apply_edit_rejected':False,'readonly':False,'last_change':None,'held':0,'held_rename':0}
try:
    with open('forbidden-server-write','w') as out: out.write('must be denied')
except OSError: metrics['readonly'] = True

def send(value):
    raw=json.dumps(value,ensure_ascii=False,separators=(',',':')).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(raw)).encode()+raw)
    sys.stdout.buffer.flush()

def diag(doc):
    result=[]
    for needle,message in [('OLD_ERROR','existing error'),('NEW_ERROR','new error'),('SECRET_ERROR','synthetic-private-value')]:
        if needle in doc['text']: result.append({'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}},'severity':1,'source':'fixture','message':message})
    return result

def push(uri):
    params={'uri':uri,'version':docs[uri]['version'],'diagnostics':diag(docs[uri])}
    if mode=='unversioned': params.pop('version')
    if mode!='pull': send({'jsonrpc':'2.0','method':'textDocument/publishDiagnostics','params':params})

def location(uri): return {'uri':uri,'range':{'start':{'line':0,'character':3},'end':{'line':0,'character':6}}}
while True:
    length=None
    while True:
        line=sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in [b'\r\n',b'\n']: break
        if line.lower().startswith(b'content-length:'): length=int(line.split(b':',1)[1])
    if length is None: sys.exit(2)
    raw=sys.stdin.buffer.read(length)
    if len(raw)!=length: sys.exit(3)
    message=json.loads(raw)
    method=message.get('method')
    if method is None:
        if message.get('id')==9000: metrics['apply_edit_rejected']=message.get('result',{}).get('applied') is False
        continue
    params=message.get('params') or {}
    request_id=message.get('id')
    if method=='initialize':
        for probe in (params.get('initializationOptions') or {}).get('read_probes',[]):
            try:
                with open(probe,'rb') as source: content=source.read(4096)
                metrics['read_probes'][probe]={'readable':True,'bytes':len(content)}
            except OSError:
                metrics['read_probes'][probe]={'readable':False,'bytes':0}
        if mode=='malformed':
            sys.stdout.buffer.write(b'Content-Length: 999999999\r\n\r\n');sys.stdout.buffer.flush();continue
        caps={'positionEncoding':'utf-8' if mode=='wrong-encoding' else 'utf-16','textDocumentSync':{'openClose':True,'change':2},'definitionProvider':True,'referencesProvider':True,'documentSymbolProvider':True,'workspaceSymbolProvider':True,'renameProvider':mode!='no-rename'}
        if mode=='pull': caps['diagnosticProvider']={'interFileDependencies':False,'workspaceDiagnostics':False}
        send({'jsonrpc':'2.0','id':request_id,'result':{'capabilities':caps,'serverInfo':{'name':'synthetic-lsp'}}})
    elif method=='initialized':
        send({'jsonrpc':'2.0','id':9000,'method':'workspace/applyEdit','params':{'edit':{'changes':{}}}})
    elif method=='textDocument/didOpen':
        doc=params['textDocument'];docs[doc['uri']]={'version':doc['version'],'text':doc['text']};metrics['opens']+=1;push(doc['uri'])
    elif method=='textDocument/didChange':
        doc=params['textDocument'];change=params['contentChanges'][0];metrics['last_change']=change;docs[doc['uri']]={'version':doc['version'],'text':change['text']};metrics['changes']+=1;push(doc['uri'])
    elif method=='textDocument/didClose': docs.pop(params['textDocument']['uri'],None)
    elif method=='textDocument/diagnostic':
        uri=params['textDocument']['uri'];doc=docs[uri];version=str(doc['version']);result={'kind':'unchanged','resultId':version} if params.get('previousResultId')==version else {'kind':'full','items':diag(doc),'resultId':version};send({'jsonrpc':'2.0','id':request_id,'result':result})
    elif method=='textDocument/definition': send({'jsonrpc':'2.0','id':request_id,'result':location(params['textDocument']['uri'])})
    elif method=='textDocument/references':
        uri=params['textDocument']['uri'];send({'jsonrpc':'2.0','id':request_id,'result':[location(uri)]*(2 if params['context']['includeDeclaration'] else 1)})
    elif method=='textDocument/documentSymbol':
        loc=location(params['textDocument']['uri']);send({'jsonrpc':'2.0','id':request_id,'result':[{'name':'synthetic_symbol','kind':12,'range':loc['range'],'selectionRange':loc['range']}]})
    elif method=='workspace/symbol':
        query=params.get('query')
        if query=='hold':
            metrics['held']+=1
            continue
        if query=='crash': sys.exit(42)
        if query=='__release_rename' and pending_rename:
            old_id,uri,version,new_name=pending_rename
            send({'jsonrpc':'2.0','id':old_id,'result':{'documentChanges':[{'textDocument':{'uri':uri,'version':version},'edits':[{'range':location(uri)['range'],'newText':new_name}]}]}})
            pending_rename=None
        send({'jsonrpc':'2.0','id':request_id,'result':metrics if query=='__metrics' else [{'name':query,'kind':12,'location':location(next(iter(docs),'file:///synthetic'))}]})
    elif method=='textDocument/rename':
        if mode=='hold-rename':
            uri=params['textDocument']['uri'];pending_rename=(request_id,uri,docs[uri]['version'],params['newName']);metrics['held_rename']+=1;continue
        uri=params['textDocument']['uri'];send({'jsonrpc':'2.0','id':request_id,'result':{'documentChanges':[{'textDocument':{'uri':uri,'version':docs[uri]['version']},'edits':[{'range':location(uri)['range'],'newText':params['newName']}]}]}})
    elif method=='$/cancelRequest': metrics['cancelled']+=1
    elif method=='shutdown': send({'jsonrpc':'2.0','id':request_id,'result':None})
    elif method=='exit': sys.exit(0)
    elif request_id is not None: send({'jsonrpc':'2.0','id':request_id,'error':{'code':-32601,'message':'unsupported fixture method'}})
