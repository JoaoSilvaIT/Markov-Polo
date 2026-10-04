use serde::Serialize;
use serde_json::Value;
use ta::indicators::{RelativeStrengthIndex, SimpleMovingAverage, AverageTrueRange};
use ta::{Next, DataItem};

#[derive(Debug, Clone, Serialize)]
pub struct Technicals {
    pub symbol: String,
    pub price: f64,
    pub rsi_14: f64,
    pub trend: String,
    pub bb_width: f64,
    pub volume_confirmed: bool,
    pub hmm_regime: i32,
    pub garch_vol_forecast: f64,
    pub perpetual_funding_rate: f64,
    pub recommended_sl: f64,
    pub recommended_tp: f64,
}

pub fn adaptive_round(val: f64) -> f64 {
    let abs_val = val.abs();
    if abs_val >= 100.0 { (val * 100.0).round() / 100.0 }
    else if abs_val >= 1.0 { (val * 1000.0).round() / 1000.0 }
    else if abs_val >= 0.01 { (val * 100000.0).round() / 100000.0 }
    else if abs_val > 0.0 { (val * 100000000.0).round() / 100000000.0 }
    else { 0.0 }
}

pub fn compute_indicators(
    symbol: &str,
    klines: &[Value],
    hmm_regime: i32,
    garch_vol_forecast: f64,
    funding_rate: f64,
) -> Technicals {
    let mut rsi = RelativeStrengthIndex::new(14).unwrap();
    let mut sma_20 = SimpleMovingAverage::new(20).unwrap();
    let mut sma_50 = SimpleMovingAverage::new(50).unwrap();
    let mut vol_sma_20 = SimpleMovingAverage::new(20).unwrap();
    let mut atr = AverageTrueRange::new(14).unwrap();
    let mut bb_mid = SimpleMovingAverage::new(20).unwrap();

    let mut rsi_val = 0.0;
    let mut sma_20_val = 0.0;
    let mut sma_50_val = 0.0;
    let mut vol_sma_20_val = 0.0;
    let mut atr_val = 0.0;
    let mut bb_mid_val = 0.0;
    
    let mut closes = Vec::new();

    let last_kline = klines.last().unwrap();
    let current_price = last_kline[4].as_str().unwrap().parse::<f64>().unwrap();
    let current_vol = last_kline[5].as_str().unwrap().parse::<f64>().unwrap();

    for k in klines {
        let high = k[2].as_str().unwrap().parse::<f64>().unwrap();
        let low = k[3].as_str().unwrap().parse::<f64>().unwrap();
        let close = k[4].as_str().unwrap().parse::<f64>().unwrap();
        let vol = k[5].as_str().unwrap().parse::<f64>().unwrap();
        closes.push(close);

        rsi_val = rsi.next(close);
        sma_20_val = sma_20.next(close);
        sma_50_val = sma_50.next(close);
        vol_sma_20_val = vol_sma_20.next(vol);
        bb_mid_val = bb_mid.next(close);
        
        let open: f64 = k[1].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let mut item = DataItem::builder()
            .open(open)
            .high(high)
            .low(low)
            .close(close)
            .volume(vol)
            .build()
            .unwrap();
        atr_val = atr.next(&item);
    }

    // Manual Bollinger Bands Width
    let len = closes.len();
    let mut bb_width = 0.0;
    if len >= 20 {
        let slice = &closes[len-20..];
        let mean = bb_mid_val;
        let mut var = 0.0;
        for &x in slice { var += (x - mean).powi(2); }
        let std = (var / 20.0).sqrt();
        let upper = mean + 2.0 * std;
        let lower = mean - 2.0 * std;
        bb_width = (upper - lower) / mean;
    }

    let trend = if current_price > sma_20_val && sma_20_val > sma_50_val {
        "strong_bullish".to_string()
    } else {
        "neutral/bearish".to_string()
    };

    Technicals {
        symbol: symbol.to_string(),
        price: current_price,
        rsi_14: (rsi_val * 10.0).round() / 10.0,
        trend,
        bb_width: (bb_width * 10000.0).round() / 10000.0,
        volume_confirmed: current_vol > vol_sma_20_val,
        hmm_regime,
        garch_vol_forecast,
        perpetual_funding_rate: funding_rate,
        recommended_sl: adaptive_round(current_price - 1.5 * atr_val),
        recommended_tp: adaptive_round(current_price + 2.0 * atr_val),
    }
}
