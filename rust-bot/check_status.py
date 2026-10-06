import os
import json
import requests
import time
import hmac
import hashlib
from urllib.parse import urlencode

# Read from ../.env manually
env_vars = {}
with open('../.env', 'r') as f:
    for line in f:
        line = line.strip()
        if line and '=' in line and not line.startswith('#'):
            k, v = line.split('=', 1)
            v = v.strip('"\'')
            env_vars[k] = v

api_key = env_vars.get('BINANCE_API_KEY')
api_secret = env_vars.get('BINANCE_SECRET_KEY')

def get_binance_data(endpoint, params=None):
    if params is None:
        params = {}
    params['timestamp'] = int(time.time() * 1000)
    query_string = urlencode(params)
    signature = hmac.new(api_secret.encode('utf-8'), query_string.encode('utf-8'), hashlib.sha256).hexdigest()
    url = f"https://api.binance.com{endpoint}?{query_string}&signature={signature}"
    headers = {'X-MBX-APIKEY': api_key}
    res = requests.get(url, headers=headers)
    return res.json()

print("=== LOCAL STATE FILES ===")
import glob
for state_file in glob.glob('../rust-bot_data/binance_hmm_state_*.json'):
    with open(state_file, 'r') as f:
        print(f"{os.path.basename(state_file)}:")
        print(json.dumps(json.load(f), indent=2))
        
print("\n=== BINANCE OPEN ORDERS ===")
open_orders = get_binance_data('/api/v3/openOrders')
if isinstance(open_orders, list):
    for o in open_orders:
        print(f"Symbol: {o['symbol']}, Side: {o['side']}, Price: {o['price']}, Qty: {o['origQty']}")
else:
    print(open_orders)

print("\n=== CURRENT PRICES ===")
for symbol in ['BTCUSDC', 'ETHUSDC', 'SOLUSDC']:
    ticker = requests.get(f"https://api.binance.com/api/v3/ticker/price?symbol={symbol}").json()
    print(f"{symbol}: {ticker.get('price')}")

