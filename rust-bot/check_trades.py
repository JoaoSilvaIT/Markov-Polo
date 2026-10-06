import os
import requests
import time
import hmac
import hashlib
from urllib.parse import urlencode

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

trades = get_binance_data('/api/v3/myTrades', {'symbol': 'BTCUSDC', 'limit': 5})
for t in trades:
    print(f"Time: {time.ctime(t['time']/1000)}, Side: {'BUY' if t['isBuyer'] else 'SELL'}, Price: {t['price']}, Qty: {t['qty']}, Maker: {t['isMaker']}")


for symbol in ['ETHUSDC', 'SOLUSDC']:
    trades = get_binance_data('/api/v3/myTrades', {'symbol': symbol, 'limit': 5})
    if isinstance(trades, list):
        for t in trades:
            print(f"Symbol: {symbol}, Time: {time.ctime(t['time']/1000)}, Side: {'BUY' if t['isBuyer'] else 'SELL'}, Price: {t['price']}, Qty: {t['qty']}, Maker: {t['isMaker']}")
