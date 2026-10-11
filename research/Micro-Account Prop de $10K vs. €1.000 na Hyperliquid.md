# Micro-Account Prop de $10K vs. €1.000 na Hyperliquid

**Data de corte:** 10 de outubro de 2026  
**Perfil:** residente fiscal em Portugal; capital líquido máximo de €1.000; sistema Hyperliquid Dual-Engine.  
**Nota:** análise de decisão e risco, não aconselhamento fiscal ou jurídico individual.

## Executive Decision

### Recomendação: **Hybrid limitado a uma única tentativa**

Faz sentido arriscar cerca de **€60–€70 uma só vez**, desde que o bot já tenha passado por replay das regras e shadow trading no venue da prop. Não faz sentido reservar €80–€120 para tentativas repetidas, nem alterar a estratégia para recuperar a fee. O orçamento vitalício inicial deve ser: **uma avaliação, sem reset e sem segunda tentativa financiada pelos €1.000**.

A melhor correspondência técnica encontrada é a **Propr Classic 1-Step de $10K**, porque opera sobre infraestrutura Hyperliquid, é API-first, permite bots, tem drawdown total estático e não impõe prazo; o preço oficial normal começa em $110, mas existe uma promoção oficial de 30% na primeira avaliação, reduzindo-o teoricamente a $77, aproximadamente €68,71 à taxa de referência de 9 de outubro de 2026. Contudo, o limite diário é 3%, não 4–5%, e a fee é contratualmente não reembolsável; portanto, **nem esta opção cumpre literalmente todos os cinco filtros**.[^1][^2][^3][^4][^5][^6]

O capital restante — aproximadamente €931 — deve ficar fora da prop. Não deve ser colocado integralmente como margem na Hyperliquid enquanto a avaliação decorre. A separação recomendada é:

- **€69:** uma avaliação Propr $10K, apenas depois dos testes.
- **€100–€150:** reserva operacional/fiscal, fora de exchanges e wallets de trading.
- **€781–€831:** capital próprio disponível para implementação gradual na Hyperliquid.

Se a promoção não estiver ativa no checkout, a decisão muda para **No-Go no $10K** e **Go condicional num $5K de $59–$69**, porque pagar $99–$129 representa aproximadamente 8,8–11,5% dos €1.000 e excede o limite pessoal proposto.[^7][^8][^9]

## €1.000 Próprios vs. $10K Prop

| Dimensão | Hyperliquid com €1.000 | Prop $10K |
|---|---:|---:|
| Perda pessoal máxima contratual | Até ao capital transferido; pode ser limitada operacionalmente, mas existe risco de protocolo, wallet e liquidação | Fee da avaliação, aproximadamente €69 no cenário Propr promocional |
| Capital nominal | €1.000 | $10.000 simulado; não equivale a capital pertencente ao trader |
| Capital de risco efetivo | Definido pelo próprio sistema | Só $600 num programa com 6% max DD; o “$10K” é sobretudo uma unidade de sizing |
| Microestrutura | Hyperliquid nativa, L2, maker/post-only | Propr é o fit mais próximo; CFT/Hyro/Dolvero dependem de simulação/API de terceiros |
| Regras externas | Nenhum target, consistency ou payout approval | Daily/max DD, breach por equity, contrato e risco de contraparte |
| Tributação provável | Perps como derivados/Categoria G, salvo requalificação profissional | Categoria B, prestação/reward contratual |
| Escala de payout | Limitada pelo capital próprio | 80% do PnL simulado elegível, depois de passar a avaliação |
| Risco de ruína pessoal | Elevado apenas se se colocar e arriscar todo o capital; evitável com segregação | 6,9% da banca por tentativa de €69; 13,8% após duas falhas |

As contas prop são contratuais e simuladas. A Propr declara que os saldos e PnL são hipotéticos e que os pagamentos são performance rewards por sinais, não retornos de investimento. HyroTrader e Crypto Fund Trader fazem divulgação semelhante sobre capital demo.[^10][^11][^12][^6][^13]

### Risco de ruína

Há duas definições diferentes:

1. **Ruína financeira pessoal:** perda dos €1.000.
2. **Ruína da estratégia/conta:** breach da conta prop ou perda do orçamento alocado à Hyperliquid.

Numa única tentativa de €68,71, a perda pessoal máxima direta é 6,87% da banca. Duas tentativas elevam-na para 13,74%; cinco tentativas consumiriam 34,36%, mesmo antes de existir qualquer payout. Por isso, a assimetria favorável só existe se houver um **hard cap de uma tentativa**.

Na Hyperliquid, a banca não precisa de estar toda em risco. Com risco de 0,50% por trade, cada stop planeado corresponde a €5 sobre €1.000; três perdas correspondem a €15 antes de custos e slippage. A diferença é que a prop elimina perdas de trading além da fee, mas introduz um target obrigatório, risco de desqualificação e risco de não pagamento.

### Break-even económico

Com profit split de 80%, um retorno funded de 1% sobre $10K produz $100 de PnL simulado e $80 de payout bruto. À taxa ECB de 1 EUR = 1,1206 USD, isso equivale a aproximadamente €71,39. Assim, um primeiro mês funded de cerca de 1% recupera economicamente uma fee promocional de €68,71, mesmo que a firma não a reembolse — mas isto só ocorre **depois** de completar a avaliação.[^4][^14]

A prop gera aproximadamente oito vezes o payout bruto de uma banca própria de €1.000 ao mesmo retorno percentual e antes de profit split; após o split, a relação é aproximadamente 7,14 vezes. Essa alavancagem económica não é gratuita: o acesso depende de atingir 9–10% durante a avaliação sem tocar nas barreiras.

## Shortlist Realista

### Não existem três opções verificadas que cumpram os cinco filtros

A pesquisa não encontrou três firmas que, simultaneamente, tenham: Portugal verificado, crypto perps não-CFD, API para bot, $10K por $60–$90, max drawdown estático e no time limit. Apresentar três “vencedoras” como se todas passassem seria incorreto.

| Firma / plano | Preço $10K | Venue/API | Drawdown e prazo | Portugal | Falha no filtro | Decisão |
|---|---:|---|---|---|---|---|
| **Propr Classic 1-Step** | $110 normal; cerca de $77 com promoção oficial de 30% na primeira avaliação[^3][^15] | REST/SDK; infraestrutura Hyperliquid; bots permitidos[^1][^2][^16] | 6% max DD estático; 3% diário; sem prazo, minimum days ou consistency[^5] | Portugal não consta das jurisdições excluídas[^12][^17] | Daily DD é 3%, fee não reembolsável | **Escolha principal, se checkout ≤ €70** |
| **Dolvero Crypto 2-Step** | $99 publicado[^18] | Bybit Demo; bots permitidos; Dolvero lê a conta por API[^19][^20] | 4% diário, 6% max estático, ilimitado; 5 qualifying days por fase e consistency de 15%[^21][^18] | Não foi obtida confirmação primária específica para Portugal | Preço > $90; regras publicadas apresentam versões contraditórias | **Não comprar sem confirmação escrita** |
| **HyroTrader 2-Step $10K** | $119 publicado; $5K começa em $59[^9][^22] | Bybit/BloFin demo por API; custom bots e scalping permitidos[^8][^23][^24] | 5% diário, 10% max; static daily; sem prazo, mas fecha após 30 dias inativo[^8][^25] | Portugal não consta da hard-block list e pagamentos EEE são processados por entidade eslovaca[^10][^26] | $10K fora do orçamento; dependência de produto terceiro | **Preferir $5K se Propr indisponível** |
| **Crypto Fund Trader 2-Phase** | Fontes apontam aproximadamente $100 no $10K; 1-Phase mais barato usa trailing DD[^27][^28] | Bybit pessoal por API, USDT futures[^29][^30] | 5% diário e 10% max fixo no 2-Phase; sem prazo[^29] | A firma não adiciona restrição, mas remete elegibilidade para Bybit[^29][^31] | Preço não confirmado dentro de $60–$90; rota barata é trailing | **Watchlist, não seleção atual** |
| **MyFundedPerps $10K** | Select desde $40, Prime desde $60, Signature desde $90[^32][^28] | REST + WebSocket próprios; order books reais com execução paper[^33][^34][^35] | Max DD estático por plano; API beta; 30 dias de inatividade na avaliação e 7 dias funded[^28][^36] | Não foi encontrada confirmação primária suficiente para KYC/payout de residente português | Não executa diretamente na conta Hyperliquid do trader; API beta; elegibilidade PT não verificada | **Candidato técnico, pendente de compliance** |

### Problema Bybit em Portugal

As firmas que exigem Bybit Global merecem cautela adicional. A Bybit direcionou novos residentes do EEE para a Bybit EU; a oferta europeia tem disponibilidade de produtos distinta, e a própria documentação diz que apenas as funcionalidades disponíveis na Bybit EU aparecem na API. A página demo europeia confirma API algorítmica, mas outra documentação oficial descreve o demo europeu como spot; não foi possível confirmar que um residente português consegue hoje ligar os USDT perpetuals exigidos por HyroTrader, CFT ou Dolvero sem contornar restrições.[^37][^38][^39][^40][^41]

Não usar VPN nem uma conta Global indevidamente. Antes da compra, pedir à prop confirmação escrita de que **Bybit EU Portugal + demo USDT perpetual API** é suportado. Se não for, usar apenas uma integração alternativa contratualmente autorizada — por exemplo, BloFin na HyroTrader — depois de confirmar a elegibilidade portuguesa.

### Escolha prática

1. **Primeiro:** Propr $10K Classic 1-Step se o total final for ≤ €70. É o melhor fit para a arquitetura Hyperliquid e não depende de Bybit.
2. **Alternativa de menor risco:** HyroTrader $5K Two-Step por $59, e não $10K, se a integração autorizada funcionar em Portugal.
3. **Aguardar:** MyFundedPerps até existir confirmação escrita de Portugal, estabilidade da API e semantics suficientes para o Engine A.

## Risk Sizing do $10K

### Não existe percentagem que “garanta” ausência de breach

Nenhum risk-per-trade garante matematicamente que a conta nunca toca em -4%. Stops podem sofrer slippage, gaps, funding, partial fills, falha de API e mark-price divergence. A garantia só existe se a posição máxima for zero. O objetivo prático é manter a perda planeada de três trades muito abaixo da barreira, deixando reserva para esses riscos.

### Parametrização recomendada

| Parâmetro | Engine A | Engine B |
|---|---:|---:|
| Risco máximo planeado por trade | **0,50% = $50** | **0,75% = $75** |
| Perda após 3 stops iguais | 1,50% = $150 | 2,25% = $225 |
| Folga até daily limit de 4% | $250 | $175 |
| Folga até daily limit de 3% da Propr | $150 | $75 |
| Máximo de perdas completas/dia | 3 | 2 na Propr; 3 apenas em programa com 4–5% diário |
| Stop diário interno | -1,75% | -1,75% combinado; hard kill a -2,00% |

Para a Propr, cuja daily barrier é 3%, três perdas de Engine B a 0,75% deixam apenas 0,75% para slippage e custos. Logo, a regra operacional deve ser **duas perdas B por dia**, ou reduzir B para 0,60% se se pretender permitir três: três stops seriam 1,80%, deixando 1,20% de buffer.[^5]

### Configuração final sugerida

- **Engine A:** 0,50% de risco total até ao stop, incluindo fee, funding e slippage stressado.
- **Engine B:** 0,60% na Propr; 0,75% apenas em programas com daily loss de 4–5%.
- **Mixed sequence:** máximo de três stops por sessão, com `daily_stop = -1,75%`.
- **Hard kill:** rejeitar novas entradas a -1,75%; cancelar resting orders; fechar risco residual antes de -2,00%.
- **Breach buffer:** nunca usar os últimos 100 bps da barreira oficial como capacidade negociável.
- **Cluster lock:** manter uma posição global; não aumentar risco porque BTC, ETH, SOL e AVAX parecem “sinais separados”.

O sizing deve usar risco de equity, não margem:

\[
\text{Notional} = \frac{E \times r}{d_{stop}+c_{fees}+c_{slippage}+c_{funding}}
\]

onde \(E\) é equity, \(r\) é 0,005 no Engine A ou 0,006 no Engine B/Propr, e os custos são percentagens do notional. A alavancagem apenas determina margem; não altera a perda no stop.

## Tempo de Aprovação

### Não calculável com rigor sem o histórico

O risk sizing, isoladamente, não informa o drift nem a taxa de chegada de trades. São necessários pelo menos 12 meses de retornos líquidos por trade, timestamps, MAE/MFE, tempo em Neutral, fees e slippage no venue da prop. Sem isso, não existe um expected time-to-pass estatístico honesto.

A tabela seguinte é apenas uma **sensibilidade aritmética**, não uma previsão. Assume retorno médio mensal líquido da conta estável, ignora variação e breaches e divide o target pelo retorno médio:

| Retorno médio da conta/mês | 10% numa fase | 10% + 5% em duas fases | 8% + 5% em duas fases |
|---:|---:|---:|---:|
| 0,5% | 20 meses | 30 meses | 26 meses |
| 1,0% | 10 meses | 15 meses | 13 meses |
| 2,0% | 5 meses | 7,5 meses | 6,5 meses |
| 3,0% | 3,3 meses | 5 meses | 4,3 meses |
| 4,0% | 2,5 meses | 3,75 meses | 3,25 meses |

Com uma estratégia capaz de ficar 48–72 horas em Neutral, um intervalo plausível só pode ser derivado do backtest. O procedimento correto é aplicar o rulebook trade a trade e fazer block bootstrap por regimes HMM para medir:

- probabilidade de atingir o target antes do breach;
- mediana de dias corridos condicionada a aprovação;
- percentis 25/75 e 10/90;
- probabilidade de breach diário e total;
- número esperado de tentativas e fee burn.

## Payout Mensal Líquido

### Premissas

- Conta nominal: $10.000.
- Profit split: 80%.
- Conversão: 1 USD = aproximadamente €0,89238, com base na referência ECB de 9 de outubro de 2026.[^42][^4]
- Categoria B em regime simplificado.
- Coeficiente 0,35 **assumido**, não garantido; o CIRS aplica 0,35 a prestações de serviços não previstas nas categorias anteriores.[^43][^44]
- Cenário IRS ilustrativo: taxa marginal de 12,5%, correspondente ao primeiro escalão de 2026. A taxa efetiva real depende do rendimento total, deduções e agregado.[^45][^46]
- Segurança Social normal: 21,4% sobre 70% da receita de serviços, equivalente aproximadamente a 14,98% da faturação antes de opções e isenções.[^47]

### Fórmula

Para retorno funded mensal \(m\):

\[
P_{EUR}=10.000\times m\times 0,80\times 0,89238
\]

Com coeficiente 0,35, taxa marginal de IRS \(t\) e Segurança Social normal aproximada:

\[
P_{liquido}\approx P_{EUR}\times(1-0,35t-0,1498)
\]

### Cenários mensais

| Retorno da conta | PnL da conta | Payout 80% | Payout em EUR | Líquido estimado com IRS 12,5% + SS | Líquido com IRS 12,5% e sem SS temporária |
|---:|---:|---:|---:|---:|---:|
| 0,5% | $50 | $40 | €35,70 | **€28,79** | **€34,13** |
| 1,0% | $100 | $80 | €71,39 | **€57,57** | **€68,27** |
| 2,0% | $200 | $160 | €142,78 | **€115,14** | **€136,54** |
| 3,0% | $300 | $240 | €214,17 | **€172,71** | **€204,81** |
| 4,0% | $400 | $320 | €285,56 | **€230,27** | **€273,08** |

Estes números não incluem IVA, porque numa prestação B2B a cliente estrangeiro a operação pode ser não localizada em Portugal/reverse charge conforme sede e estatuto do adquirente; também não incluem custos contabilísticos ou de conversão.

Se a taxa marginal for 21,2%, o líquido com Segurança Social cai para aproximadamente 77,6% do payout em euros, em vez de 80,645%. O IRS não é simplesmente “35% × 12,5%” em todos os casos: o coeficiente define rendimento tributável e esse rendimento é agregado aos restantes rendimentos.

### Fee reembolsada

A premissa de reembolso não se aplica à Propr: a firma qualifica as registration fees como não reembolsáveis. HyroTrader devolve o depósito de One-Step/Two-Step com o primeiro payout, desde que exista pelo menos $100 de lucro após split. Dolvero publica informações inconsistentes entre primeiro e segundo payout, pelo que a data de reembolso deve constar do acordo assinado.[^48][^49][^18][^6][^7]

Fiscalmente, se a quantia for juridicamente uma devolução de depósito, não deve ser automaticamente tratada como reward; se for reembolso de fee/custo, o tratamento pode diferir. Guardar a fatura inicial e o comprovativo separado do reembolso e validar com contabilista.

## Roadmap em Três Fases

### Fase 1 — Setup e avaliação

**Budget máximo:** €70 all-in.

1. Confirmar no checkout Propr Classic 1-Step $10K por ≤ €70; se exceder, não comprar e avaliar apenas um $5K entre $59 e $69.
2. Guardar PDF/hash dos Terms, Rulebook, price e promoção.
3. Confirmar KYC português e método de payout antes do pagamento.
4. Abrir atividade antes de prestar o primeiro serviço remunerado; usar CIRS 1519/coeficiente 0,35 apenas após validação do enquadramento.
5. Criar adapter Propr separado; não enviar a chave nativa da wallet Hyperliquid.
6. Fazer 30 dias de sandbox/shadow replay com os limites Propr.
7. Configurar A = 0,50%, B = 0,60%, daily stop -1,75%, hard kill -2,00% e no máximo três stops/dia.
8. Comprar uma única avaliação. Se falhar, parar e produzir post-mortem; sem reset e sem nova fee retirada dos €931.

### Fase 2 — Funded e payout

1. Manter exatamente os mesmos signals e sizing; não aumentar risco depois de passar.
2. Pedir o primeiro payout assim que o valor líquido justificar as fees de conversão; na Propr, os payouts são on-demand e retiram todo o lucro disponível, resetando saldo e drawdown.[^50]
3. Emitir fatura/recibo pelo reward em Categoria B, converter para EUR na data relevante e guardar TXID, statement, reward approval e contrato.
4. Reservar **25% do payout** até o enquadramento fiscal e contributivo estar confirmado; elevar para 30% se já existir outro rendimento tributável.
5. O primeiro objetivo funded não é “salário”: é recuperar economicamente a fee e validar que o payout chega.

### Fase 3 — Construção própria

Distribuir cada payout líquido:

- **70% para capital próprio Hyperliquid**, até a banca própria atingir €2.000.
- **20% para reserva fiscal/operacional**, ajustada após liquidação real de IRS e Segurança Social.
- **10% para infraestrutura**, logs, VPS dedicado e dados — nunca para comprar imediatamente outro challenge.

Quando a banca própria atingir €2.000, rever a divisão para 60% Hyperliquid, 25% reserva/cash e 15% infraestrutura. Só considerar uma segunda conta prop depois de três payouts confirmados e de a soma total de fees históricas representar menos de 5% dos payouts líquidos acumulados.

## Decisão Final

**Go condicional para uma única micro-avaliação, mas não para qualquer $10K.** A compra racional é a Propr Classic 1-Step apenas se a promoção colocar o custo final em aproximadamente €69; tecnicamente, é a opção mais próxima da arquitetura atual. O preço normal de $110 é elevado para uma banca de €1.000 e não deve ser aceite.

**Risk sizing recomendado:** Engine A 0,50%; Engine B 0,60% na Propr ou 0,75% apenas sob daily DD de 4–5%; daily stop interno -1,75%; hard kill -2,00%. Isto não garante ausência de breach, mas mantém três perdas planeadas dentro de uma margem defensiva.

**Expected time-to-pass:** não estimável até a equipa fornecer o histórico de trades. Se o sistema líquido gerar 2% por mês nas regras da prop, a aritmética sem risco sugere cerca de cinco meses para uma fase de 10% e 7,5 meses para 10% + 5%; a duração estatística real será maior e deve ser calculada por bootstrap de regimes.

**Se o bot ainda não produziu pelo menos 6–12 meses de dados executáveis com custos:** não comprar challenge. Operar em shadow/demo e preservar os €1.000 tem maior valor esperado do que pagar para descobrir se a estratégia é compatível.

---

## References

1. [Developer Docs | Trading API | Propr](https://www.propr.xyz/developers)

2. [The Crypto Prop FirmBuilt Onchain](https://www.propr.xyz/)

3. [Try Propr | 30% Off Your First Crypto Prop Firm Challenge](https://www.propr.xyz/try-propr)

4. [Euro exchange rates charts](https://www.ecb.europa.eu/stats/policy_and_exchange_rates/euro_reference_exchange_rates/html/eurofxref-graph-usd.en.html) - The European Central Bank (ECB) is the central bank of the European Union countries which have adopt...

5. [Trading Rules and Evaluation](https://www.propr.xyz/rules) - Classic 1-Step: Reach the 10% profit target while staying within the drawdown limits (3% daily loss,...

6. [Risk Disclaimer and Disclosures - Propr](https://www.propr.xyz/risk-disclaimer)

7. [HyroTrader - Crypto Prop Firm | Funded Accounts up to $200K](https://www.hyrotrader.com/) - Award-winning crypto prop firm paying an 80% profit split, up to 90% as you scale. Profit split - 80...

8. [How to Start Crypto Prop Trading](https://www.hyrotrader.com/how-to-start/) - Pick a Zero-Step, One-Step or Two-Step challenge, hit the 10% target (plus 5% in Verification on Two...

9. [velotrade.com › blog › hyrotrader-reviewHyroTrader Review 2026 | Crypto Prop Firm | Velotrade](https://velotrade.com/blog/hyrotrader-review) - HyroTrader review 2026: challenge structure, trailing daily drawdown, 40% consistency rule, profit s...

10. [FAQ - How can we help you?](https://www.hyrotrader.com/faq/) - hyrotrader.com is a platform for trading crypto with Hyro

11. [Crypto Fund Trader - ﻿ Best Trading Prop Firm | Up to $300.000](https://cryptofundtrader.com/) - Crypto Fund Trader is a prop firm with the best market conditions. No spread, low commission and mor...

12. [PROPR Service Provision and Funded Account Terms](https://www.propr.xyz/funded-terms)

13. [How it Works - Propr](https://www.propr.xyz/docs/how-it-works)

14. [Euro foreign exchange reference rates - European Central Bank](https://www.ecb.europa.eu/stats/policy_and_exchange_rates/euro_reference_exchange_rates/html/index.et.html) - The euro foreign exchange reference rates (also known as the ECB reference rates) are published by t...

15. [Revenue Model - Crypto Prop Firm on Hyperliquid - Propr](https://www.propr.xyz/docs/revenue-model) - The leading crypto prop firm built on Hyperliquid. Get funded up to $100K to trade perpetual futures...

16. [Python SDK](https://www.propr.xyz/docs/bot/python-sdk) - The leading crypto prop firm built on Hyperliquid. Get funded up to $100K to trade perpetual futures...

17. [Terms & Conditions](https://www.propr.xyz/terms)

18. [How Prop Trading Works — Complete Evaluation Guide](https://dolvero.com/howitworks) - 2-step challenge with 10% and 5% profit targets, no time limits, and 24-hour payouts.

19. [Launching Crypto Challenges: 700+ Pairs, 50x Leverage, 24/7 Markets](https://dolvero.com/blog/launching-crypto-challenges) - Dolvero crypto challenges are live on Bybit Demo. 700+ USDT-margined perpetual contracts, 50x levera...

20. [How to Connect Your Bybit Demo to Dolvero: A Step-by- ...](https://dolvero.com/blog/how-to-connect-bybit-demo-dolvero) - Connecting your Bybit demo account to Dolvero takes about 10 minutes. This is the exact step-by-step...

21. [Crypto vs. CFD Evaluation at Dolvero: Which Rules Differ](https://dolvero.com/blog/crypto-vs-cfd-evaluation-dolvero?lang=en) - Crypto challenges and CFD forex evaluations share a common DNA but differ in critical ways. This sid...

22. [HyroTrader Challenges & Pricing 2026 | PropFirmWay](https://propfirmway.com/prop-firms/hyrotrader/challenges) - Compare all HyroTrader challenge options, account sizes, pricing and profit splits. Find the best fu...

23. [www.hyrotrader.com › evaluationsCrypto Prop Trading Evaluation Options - HyroTrader](https://www.hyrotrader.com/evaluations/) - HyroTrader’s crypto challenge is designed to evaluate your trading skills and discipline through a Z...

24. [Best Prop Firm for Crypto Futures​ - HyroTrader](https://www.hyrotrader.com/blog/best-prop-firm-for-crypto-futures/) - Best prop firm for crypto futures: what USDT perpetual traders need, live market data, deep pairs, s...

25. [What is the difference between Standard (Trailing) and Swing (Fixed ...](https://www.hyrotrader.com/faq/rules/what-is-the-difference-between-standard-trailing-and-swing-fixed-daily-drawdown/) - hyrotrader.com is a platform for trading crypto with Hyro

26. [Terms and Conditions - HyroTrader](https://www.hyrotrader.com/terms-and-conditions/) - hyrotrader.com is a platform for trading crypto with Hyro

27. [propfirmsradar.com · prop-firm · cryptofundtraderCrypto Fund Trader Review 2026: Rules, Payouts & the Bybit Route](https://propfirmsradar.com/prop-firm/cryptofundtrader) - Crypto Fund Trader review 2026. 715+ crypto pairs on Bybit, MT5 or Match-Trader, a flat 80% split wi...

28. [Perp Prop Firms: Markets, Rules and Payouts | OptimistFi](https://optimistfi.com/perps/props) - 9 prop firms that fund crypto and stock perps traders, compared: drawdown and daily loss rules, leve...

29. [FAQ](https://cryptofundtrader.com/faq/) - Crypto Fund Trader is a simulated trading evaluation firm that gives traders the opportunity to prov...

30. [CFT X Bybit](https://cryptofundtrader.com/bybit/) - Revolutionize the way you trade. Get evaluated on Bybit with real market conditions and generate rea...

31. [Terms and Conditions](https://cryptofundtrader.com/terms-and-conditions/) - During demo trading on the Trading Platform, you may carry out any transaction, unless they constitu...

32. [MyFundedPerps Expands Prop Trading Beyond Crypto with Multi ...](https://markets.businessinsider.com/news/currencies/myfundedperps-expands-prop-trading-beyond-crypto-with-multi-asset-perpetual-futures-infrastructure-1036593395) - Dallas, USA, October 2nd, 2026, FinanceWireThe prop firm model is changing, and MyFundedPerps is one...

33. [Trading with the API - MyFundedPerps Documentation](https://docs.myfundedperpetuals.com/trading-with-api)

34. [docs.myfundedperpetuals.com · api-capabilitiesAPI capabilities - MyFundedPerps Documentation](https://docs.myfundedperpetuals.com/api-capabilities)

35. [docs.myfundedperpetuals.comMyFundedPerps Documentation - MyFundedPerps Documentation](https://docs.myfundedperpetuals.com/)

36. [docs.myfundedperpetuals.com · trading-policyAccount trading policy - MyFundedPerps Documentation](https://docs.myfundedperpetuals.com/trading-policy)

37. [demo trading - Practice Crypto Risk-Free - bybit.eu](https://www.bybit.eu/pt-EU/derivative-activity/demo-trading/) - Practice crypto trading risk-free with our demo account. Suitable for all levels, from beginners to ...

38. [www.bybit.eu · en-EU · help-centerHow to Retrieve API Documentation - Help Center - bybit.eu](https://www.bybit.eu/en-EU/help-center/article/How-to-retrieve-API-documentations) - How to Retrieve API Documentation - Help Center

39. [Como usar a Bybit EU Demo Trading (passo a passo)](https://www.bybit.eu/pt-EU/learn/bybit-guide/how-to-use-bybit-eu-demo-trading) - Learn como usar a Bybit EU Demo Trading passo a passo, alterne entre os modos demo e ao vivo, gerenc...

40. [www.bybit.eu · en-EU · learnWhat European crypto users should know about Bybit EU and MiCAR?](https://www.bybit.eu/en-EU/learn/regulation-and-micar/what-european-crypto-users-should-know-about-bybit-eu-and-micar) - MiCAR is intended to support greater legal certainty, transparency and consumer protection in the cr...

41. [Bybit EU e MiCAR: O que os traders europeus precisam saber](https://learn.bybit.com/pt-PT/regulations/bybit-europe-eu-and-micar) - Este artigo explorará o que a MiCAR implica, detalhará a jornada da Bybit EU para obter essa licença...

42. [[PDF] ECB euro foreign exchange reference rates](https://www.ecb.europa.eu/stats/shared/pdf/eurofxref.pdf?62a6c01f99dd18c0e6f2bb83ed94c836)

43. [Artigo 31.º - Portal das Finanças](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/cirs_rep/Pages/irs31.aspx)

44. [Artigo 31.º Regime simplificado](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/cirs_rep/ra/Pages/irs31ra_202503.aspx)

45. [CAPÍTULO III Taxas Artigo 68.º Taxas gerais](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/cirs_rep/Pages/irs68.aspx)

46. [Sistema Fiscal Português - Taxas aplicáveis 2026](https://info.portaldasfinancas.gov.pt/pt/apoio_contribuinte/Folhetos_informativos/Documents/SFP-Taxas-2026.pdf)

47. [GUIA PRÁTICO - Segurança Social](https://www.seg-social.pt/storage1/files/1009---Novo-Regime-dos-Trabalhadores-Independentes-v1-09-8TvHiLaPC6okosI9oUn9_A.pdf)

48. [How to start? - HyroTrader](https://www.hyrotrader.com/faq/getting-started/how-to-start/) - hyrotrader.com is a platform for trading crypto with Hyro

49. [Dolvero | CFD, Crypto & Prediction Market Prop Trading](https://dolvero.com/) - Explore Dolvero evaluations for forex CFDs, crypto and prediction markets. Trade simulated capital w...

50. [Payout Structure - Perps, Prediction Markets & Memecoins - Propr](https://www.propr.xyz/docs/payouts) - The leading crypto prop firm built on Hyperliquid. Get funded up to $100K to trade perpetual futures...

