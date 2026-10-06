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

account = get_binance_data('/api/v3/account')
for bal in account['balances']:
    if float(bal['free']) > 0 or float(bal['locked']) > 0:
        print(f"{bal['asset']}: Free={bal['free']}, Locked={bal['locked']}")

