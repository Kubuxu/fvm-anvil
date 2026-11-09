#!/usr/bin/env python3
"""Real writable Skiff acceptance; never substitutes database rows for chain execution."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import urllib.request
import urllib.parse
import uuid

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
COMPOSE = ['docker', 'compose', '-f', str(HERE / 'compose.yaml')]

def command(args, **kw):
    result = subprocess.run([str(x) for x in args], capture_output=True, text=True, **kw)
    if result.returncode:
        # Do not expose argv: authorization commands contain the signer key.
        raise RuntimeError(f"{Path(args[0]).name} failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout.strip()

def sql(query):
    return command(COMPOSE + ['exec', '-T', 'postgres', 'psql', '-XAt', '-v', 'ON_ERROR_STOP=1', '-U', 'curio', '-d', 'curio', '-c', query])

def wait_for(fn, timeout, label):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = fn()
        if value:
            return value
        time.sleep(2)
    raise RuntimeError('timed out: ' + label)

def post(url, body):
    req = urllib.request.Request(url, json.dumps(body).encode(), {'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=60) as response:
        return response.headers.get('Location'), response.read()

def rpc(url, method, params):
    request = urllib.request.Request(url, json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params}).encode(), {'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)
    if 'error' in result:
        raise RuntimeError(f"{method}: {result['error']}")
    return result['result']

def proof_receipt(url):
    # Proof sends intentionally have no message_waits_eth row in pinned Curio.
    hashes = sql("""SELECT signed_hash FROM message_sends_eth
        WHERE send_reason='pdp-prove' AND send_success=true AND signed_hash IS NOT NULL
        UNION SELECT r.signed_hash FROM message_send_eth_replacements r
        JOIN message_sends_eth s ON s.signed_hash=r.original_signed_hash
        WHERE s.send_reason='pdp-prove' AND r.send_success=true AND r.signed_hash IS NOT NULL""")
    for tx_hash in hashes.splitlines():
        receipt = rpc(url, 'eth_getTransactionReceipt', [tx_hash])
        if receipt is not None and int(receipt['status'], 16) == 1:
            head = int(rpc(url, 'eth_blockNumber', []), 16)
            if head > int(receipt['blockNumber'], 16):
                return receipt
    return None

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--manifest', type=Path, required=True)
    p.add_argument('--private-key', required=True)
    p.add_argument('--rpc-url', default='http://127.0.0.1:18545/rpc/v1')
    p.add_argument('--service-url', default='http://127.0.0.1:18080')
    p.add_argument('--work-dir', type=Path, default=ROOT / '.curio-acceptance')
    p.add_argument('--skiff', type=Path, required=True)
    p.add_argument('--pdptool', type=Path, required=True)
    p.add_argument('--client', type=Path, required=True)
    p.add_argument('--timeout', type=int, default=1800)
    a = p.parse_args()
    service = urllib.parse.urlsplit(a.service_url)
    if service.scheme != 'http' or not service.hostname or service.path not in ('', '/'):
        p.error('--service-url must be a plain HTTP origin for the local Skiff listener')
    listener = service.netloc if service.port else service.netloc + ':80'
    a.work_dir = a.work_dir.resolve()
    a.work_dir.mkdir(parents=True, exist_ok=True)
    manifest = json.loads(a.manifest.read_text())
    env = os.environ.copy()
    env.update(manifest.get('env', manifest.get('curioEnv', {})))
    required = ['CURIO_DEVNET_PDP_VERIFIER_ADDRESS', 'CURIO_DEVNET_FWSS_ADDRESS']
    for key in required:
        if not env.get(key):
            raise RuntimeError('deployment manifest missing env.' + key)
    command(COMPOSE + ['up', '-d', '--wait', '--wait-timeout', '900'])
    command(COMPOSE + ['exec', '-T', 'cql', 'cqlsh', '-e', "CREATE KEYSPACE IF NOT EXISTS curio WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1};"])
    chain = urllib.parse.urlsplit(a.rpc_url)
    # Curio appends /rpc/v1 itself; subscriptions need a WebSocket origin.
    fullnode = urllib.parse.urlunsplit(('wss' if chain.scheme == 'https' else 'ws', chain.netloc, '', '', ''))
    env.update(FULLNODE_API_INFO=fullnode, CURIO_DB_HOST='127.0.0.1', CURIO_DB_HOST_CQL='127.0.0.1', CURIO_DB_PORT='15432', CURIO_DB_CASSANDRA_PORT='19042', CURIO_DB_NAME='curio', CURIO_DB_USER='curio', CURIO_DB_PASSWORD='curio-local-only', CURIO_DB_LOAD_BALANCE='false')
    repo = a.work_dir / 'repo'
    storage = a.work_dir / 'pieces'
    repo.mkdir(exist_ok=True)
    storage.mkdir(exist_ok=True)
    storage_meta = storage / 'sectorstore.json'
    if not storage_meta.exists():
        storage_meta.write_text(json.dumps({'ID': str(uuid.uuid4()), 'Weight': 10, 'CanSeal': True, 'CanStore': True}))
    (repo / 'storage.json').write_text(json.dumps({'StoragePaths': [{'Path': str(storage)}]}))
    args = [str(a.skiff.resolve()), '--repo-path', str(repo), '--data', str(storage)]
    log = open(a.work_dir / 'skiff.log', 'a')
    proc = subprocess.Popen(args, env=env, stdout=log, stderr=log)
    def stop():
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
    try:
        def migrated():
            if proc.poll() is not None:
                raise RuntimeError('Skiff exited during migration; inspect ' + str(a.work_dir / 'skiff.log'))
            result = subprocess.run(COMPOSE + ['exec', '-T', 'postgres', 'psql', '-XAt', '-U', 'curio', '-d', 'curio', '-c', "SELECT to_regclass('eth_keys') IS NOT NULL AND EXISTS(SELECT 1 FROM harmony_config WHERE title='base')"], capture_output=True, text=True)
            return result.returncode == 0 and result.stdout.strip() == 't'
        wait_for(migrated, 180, 'Skiff migrations')
        stop()
        if sql('SELECT count(*) FROM pdp_data_sets') != '0' or sql('SELECT count(*) FROM eth_keys') != '0':
            raise RuntimeError('acceptance requires a fresh dedicated database; docker compose down -v first')
        signer = command(['cast', 'wallet', 'address', '--private-key', a.private_key])
        key = a.private_key.removeprefix('0x')
        if not re.fullmatch('[0-9a-fA-F]{64}', key):
            raise ValueError('private key must be 32-byte hex')
        sql("INSERT INTO eth_keys(address,private_key,role) VALUES ('" + signer + "',decode('" + key + "','hex'),'pdp')")
        config = '''[Subsystems]
EnablePDP = true
EnableDealMarket = false
EnableWebGui = true
GuiAddress = "127.0.0.1:14701"
[HTTP]
Enable = true
DelegateTLS = true
ListenAddress = "127.0.0.1:18080"
DomainName = "localhost"
[Market.StorageMarketConfig.IPNI]
Disable = true
DirectAnnounceURLs = []
'''
        config = config.replace('127.0.0.1:18080', listener)
        config += '[Apis]\nStorageRPCSecret = "' + __import__('base64').b64encode(os.urandom(32)).decode() + '"\n'
        sql("UPDATE harmony_config SET config = $acceptance$" + config + "$acceptance$ WHERE title='base'")
        proc = subprocess.Popen(args, env=env, stdout=log, stderr=log)
        def ready():
            if proc.poll() is not None:
                raise RuntimeError('Skiff exited; inspect log')
            try:
                with urllib.request.urlopen(a.service_url + '/pdp/ping', timeout=2) as response:
                    return response.status == 200
            except OSError:
                return False
        wait_for(ready, 180, 'writable PDP HTTP')
        auth_path = a.work_dir / 'auth.json'
        authorize = ['python3', HERE / 'authorize.py', '--manifest', a.manifest.resolve(), '--rpc-url', a.rpc_url, '--private-key', a.private_key, '--provider-address', signer, '--service-url', a.service_url, '--output', auth_path]
        command(authorize)
        auth = json.loads(auth_path.read_text())
        loc, _ = post(a.service_url + '/pdp/data-sets', {'recordKeeper': env['CURIO_DEVNET_FWSS_ADDRESS'], 'extraData': auth['createExtraData']})
        create_hash = loc.rsplit('/', 1)[-1]
        dataset = wait_for(lambda: sql("SELECT id FROM pdp_data_sets WHERE create_message_hash='" + create_hash + "'"), a.timeout, 'event-derived dataset ID')
        if not dataset.isdecimal():
            raise RuntimeError('invalid event-derived dataset ID')
        piece_file = a.work_dir / 'piece.bin'
        piece_file.write_bytes(os.urandom(64 * 1024 * 1024))
        output = command([a.pdptool.resolve(), 'upload-piece', '--service-url', a.service_url, '--service-name', 'public', '--hash-type', 'commp', piece_file])
        cid = re.search(r'Piece CID: (\S+)', output).group(1)
        command(authorize + ['--operation', 'add', '--data-set-id', dataset, '--piece-cids', cid])
        auth = json.loads(auth_path.read_text())
        loc, _ = post(a.service_url + '/pdp/data-sets/' + dataset + '/pieces', {'pieces': [{'pieceCid': cid, 'subPieces': [{'subPieceCid': cid}]}], 'extraData': auth['addExtraData']})
        add_hash = loc.rsplit('/', 1)[-1]
        piece_id = wait_for(lambda: sql("SELECT piece_id FROM pdp_data_set_pieces WHERE data_set=" + dataset + " AND add_message_hash='" + add_hash + "'"), a.timeout, 'event-derived piece ID')
        proof = wait_for(lambda: proof_receipt(a.rpc_url), a.timeout, 'successful real provePossession')
        proof_hash = proof['transactionHash']
        (a.work_dir / 'proof-receipt.json').write_text(json.dumps(proof, indent=2) + '\n')
        if sql('SELECT count(*) FROM pdp_piecerefs WHERE needs_ipni OR ipni_task_id IS NOT NULL') != '0':
            raise RuntimeError('PDP advertisement task unexpectedly requested')
        cache = command(COMPOSE + ['exec', '-T', 'cql', 'cqlsh', '-e', 'SELECT count(*) FROM curio.pdp_cache_layer;'])
        (a.work_dir / 'proof-cache.txt').write_text(cache + '\n')
        if not re.search(r'\n\s*[1-9][0-9]*\s*\n', cache):
            raise RuntimeError('CQL proof cache is empty')
        if sql('SELECT count(*) FROM pdp_data_sets WHERE ipfs_indexing IS DISTINCT FROM false') != '0' or sql('SELECT count(*) FROM ipni') != '0':
            raise RuntimeError('indexing isolation violated')
        result = {'signer': signer, 'dataSetId': dataset, 'pieceId': piece_id, 'pieceCid': cid, 'createTx': create_hash, 'addTx': add_hash, 'proofTx': proof_hash}
        (a.work_dir / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        # Stop scheduling before the destructive snapshot/revert wire gate.
        stop()
        command([a.client.resolve(), a.rpc_url.replace('http://', 'ws://').replace('https://', 'wss://'), create_hash, add_hash, proof_hash])
        print(json.dumps(result, indent=2))
    finally:
        stop()
        log.close()

if __name__ == '__main__':
    main()
