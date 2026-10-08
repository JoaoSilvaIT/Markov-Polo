# Auditoria Quantitativa: Dual-Engine, Transições de Regime e Universo Hyperliquid

## Resumo executivo

A decisão correta no **Day 1** é não recalibrar parâmetros para “corrigir” um único choque, mas também não interpretar 17 horas sem trades como validação de que o controlador está bem calibrado. Com zero eventos em 17 horas, o intervalo de incerteza sobre a taxa real de oportunidades continua enorme: sob um modelo de Poisson, o limite superior unilateral aproximado de 95% é cerca de 4,2 oportunidades por dia. Portanto, a amostra atual não distingue paciência estatística de bloqueio estrutural.

O choque de ETH a -8,5 desvios-padrão não deve ser lido literalmente como um evento gaussiano quase impossível. Retornos cripto são fortemente não normais, leptocúrticos e assimétricos; modelos gaussianos subestimam a cauda e o risco de perda extrema. O resultado observado — ficar em caixa — constitui uma **validação funcional do veto**, não uma validação estatística dos limiares 3,0, 1,5 ou 2,0.[^1][^2]

A recomendação é uma alteração controlada em **shadow mode**, não uma edição oportunista do live engine:

- Preservar o Fast-Sentinel e o mínimo absoluto de duas barras de cooldown.
- Separar a recuperação por engine: Mean Reversion continua bloqueada até normalização; Trend pode rearmar mais cedo, mas apenas com breakout, OBI persistente, livro saudável e espaço de risco ainda disponível.
- Substituir o reset binário de streaks por acumulação de evidência com fuga, histerese e penalização assimétrica.
- Testar dwell assimétrico: 1–2 barras para Trend e 2–3 para Consolidação.
- Adicionar **AVAX primeiro em shadow**, e **SUI como segundo candidato condicional**; manter DOGE e ARB fora do universo live por agora.
- Não usar “primeiro processo a obter o lock”. Um coordenador deve reunir sinais no mesmo fecho de 15 minutos, classificar oportunidades e só depois conceder o único lease.

## Day 1: diagnóstico científico

### O que 17 horas dizem

Se a taxa verdadeira for uma oportunidade por dia, a probabilidade de observar zero em 17 horas é aproximadamente 49%; para duas por dia é 24%; para quatro por dia ainda é cerca de 6%. Assim, zero trades no primeiro período não é anómalo o suficiente para justificar alteração live.

A frequência esperada **não pode ser calculada a partir dos thresholds isoladamente**. É necessário conhecer a distribuição conjunta e a autocorrelação de:

- Posteriores HMM \(P_C\), \(P_T\) e \(P_N\).
- \(Z_{15}\) e \(Z_{60}\).
- Breakout Donchian por direção.
- OBI/OFI, spread, profundidade e staleness.
- Estado do cooldown, posição já ativa e disponibilidade do cluster lock.

Uma aproximação independente como \(q^3\), em que \(q\) é a probabilidade de uma barra passar o threshold, será enganadora porque os posteriores HMM e a volatilidade são serialmente dependentes. A métrica correta é a **hazard empírica de elegibilidade**, estimada por replay causal e por estado.

### Bloqueio estrutural possível

A arquitetura contém várias conjunções: posterior elevado, três confirmações consecutivas, veto de choque, gate de volatilidade, sinal do engine, microestrutura válida e lock livre. Mesmo que cada filtro pareça moderado isoladamente, a interseção pode tornar-se rara. O problema mais suspeito não é o cooldown de 30 minutos; é o acoplamento entre:

1. Um filtro lento de 60 minutos.
2. Reset total por uma única barra em deadband.
3. Dwell simétrico de 45 minutos.
4. Breakouts cujo valor económico pode decair enquanto o controlador espera.

Há evidência de estrutura intrahorária em cripto: volatilidade e volume têm impulsos no início de cada hora e após os minutos 15, 30 e 45, precisamente nas fronteiras usadas pelo sistema. Também existe evidência de momentum intradiário em Bitcoin em períodos de maior volume e volatilidade. Isto não prova que um dwell de 45 minutos seja errado, mas torna obrigatório medir o custo de atraso em barras e ATR, em vez de assumir que mais confirmação é sempre melhor.[^3][^4][^5]

## Frequência e instrumentação

### Funil de elegibilidade

Cada fecho de barra deve produzir um registo contrafactual com os seguintes contadores, por ativo e engine:

1. `bars_total`.
2. `hmm_threshold_pass`.
3. `evidence_accumulator_pass`.
4. `shock_veto_pass`.
5. `volatility_gate_pass`.
6. `engine_signal_pass`.
7. `obi_confirmation_pass`.
8. `book_health_pass`.
9. `portfolio_lock_available`.
10. `selected_by_portfolio_ranker`.
11. `would_submit`.
12. `would_fill`, usando simulação de fila conservadora.

Sem este funil, “zero trades” não identifica o constrangimento dominante. O relatório diário deve apresentar a taxa marginal de passagem e a taxa condicional após cada gate, além da razão concreta da primeira rejeição.

### Métricas mínimas

A calibração deve reportar:

- Oportunidades elegíveis por ativo-dia e por engine-dia.
- Tempo mediano desde o primeiro posterior favorável até à autorização.
- Movimento já consumido antes da entrada, em ATR e em percentagem do canal.
- Probabilidade de continuação a 1, 2, 3, 4 e 8 barras após autorização.
- Excursão favorável e adversa máxima após sinal.
- Ocupação do cluster lock e oportunidades perdidas durante ocupação.
- Sobreposição temporal de sinais entre ativos.
- Fill rate maker, adverse selection e PnL líquido de fees, funding e slippage.

Como teste operacional preliminar, aproximadamente 5.000 velas recentes de 15 minutos por ativo — cerca de 52 dias — exibiram muitos breakouts Donchian de 20 barras em bruto, aproximadamente 8–10 por dia, mas follow-through mediano negativo após três barras na amostra. Estes números **não são trades** e não incluem HMM, OBI, custos nem execução; servem apenas para mostrar que o canal isolado não é o principal fator de escassez e que esperar três barras pode consumir o edge.

A API oficial limita respostas por intervalo e o arquivo histórico pode ter atrasos ou lacunas; os snapshots L2 estão disponíveis, mas a documentação não garante atualização atempada nem completude. Uma conclusão robusta exige o próprio recorder contínuo do bot e replay point-in-time.[^6][^7]

## Dwell assimétrico

### Princípio

Consolidação e tendência têm custos de erro diferentes. Uma falsa classificação de consolidação pode colocar ordens contra um movimento nascente; por isso, a entrada de Mean Reversion deve continuar lenta. Uma tendência corretamente identificada mas autorizada tarde perde convexidade e piora a relação recompensa/stop; por isso, Trend deve poder entrar mais depressa quando existe confirmação independente de microestrutura.

HMMs padrão impõem durações geométricas e sem memória; Hidden Semi-Markov Models modelam explicitamente a duração do regime e são mais apropriados quando o tempo de permanência contém informação. A solução de longo prazo é um HSMM ou duration-aware HMM, não uma proliferação permanente de streak counters externos.[^8][^9]

### Grelha recomendada

Os valores seguintes são **hipóteses para shadow A/B**, não novos parâmetros live:

| Estado candidato | Confiança | Confirmação adicional | Dwell inicial a testar |
|---|---:|---|---:|
| Trend forte | \(P_T \ge 0,90\) | Donchian válido, OBI/OFI alinhado e persistente, livro saudável | 1 barra |
| Trend moderada | \(0,82 \le P_T < 0,90\) | Mesmas confirmações | 2 barras |
| Consolidação forte | \(P_C \ge 0,80\) | Gates de volatilidade e estabilidade O-U | 2 barras |
| Consolidação moderada | \(0,72 \le P_C < 0,80\) | Gates de volatilidade e estabilidade O-U | 3 barras equivalentes de evidência |
| Deadband | nenhum dominante | — | NEUTRAL, mas sem apagar toda a memória |

A grelha deve incluir o baseline atual \((3,3)\), além de \((1,2)\), \((2,2)\), \((1,3)\) e \((2,3)\), em que o primeiro elemento é dwell de Trend e o segundo de Consolidação. O critério de escolha não deve ser número de trades; deve ser expectancy líquido, Expected Shortfall, atraso em ATR e Deflated Sharpe Ratio.

## Acumulador de evidência

### Formulação recomendada

Um leaky log-odds accumulator preserva evidência parcial sem transformar uma barra cinzenta numa autorização automática:

\[
L^T_t = \rho_T L^T_{t-1} + \operatorname{clip}\left(\log\frac{P_T(t)+\epsilon}{P_C(t)+P_N(t)+\epsilon}, -c, c\right) + \beta I^{OBI}_t
\]

\[
L^C_t = \rho_C L^C_{t-1} + \operatorname{clip}\left(\log\frac{P_C(t)+\epsilon}{P_T(t)+P_N(t)+\epsilon}, -c, c\right)
\]

Para a primeira grelha, testar meia-vida de 2 barras para Trend, equivalente a \(\rho_T \approx 0,71\), e 3–4 barras para Consolidação, equivalente a \(\rho_C\) entre aproximadamente 0,79 e 0,84. O termo OBI deve usar uma feature normalizada, limitada e persistente, nunca um snapshot cru.

### Histerese

Usar dois limiares por estado:

- Entrar em Trend quando \(L^T_t \ge H_T^{on}\).
- Permanecer em Trend enquanto \(L^T_t > H_T^{off}\), com \(H_T^{off} < H_T^{on}\).
- Aplicar lógica análoga à Consolidação.
- Se ambos falharem, regressar a NEUTRAL.
- Num choque duro, zerar imediatamente os acumuladores e iniciar cooldown; o leaky integrator não deve atravessar um evento de cauda.

Isto resolve o caso \(P_C=0,73\): a barra reduz a convicção, mas não apaga duas barras anteriores. Ao contrário de uma EWMA simples do posterior, log-odds acumula evidência relativa contra estados alternativos e permite penalizações distintas para contradição forte e deadband.

### Proteção contra latência

A aceleração de Trend precisa de quatro condições adicionais:

- Breakout calculado apenas com barras anteriores e confirmado no fecho atual.
- OBI/OFI causal, com timestamps, controlo de gaps e staleness.
- `residual_move_budget > 0`: distância ao Chandelier/stop e alvo implícito ainda oferece recompensa mínima líquida.
- Nenhuma entrada se o preço já percorreu uma fração excessiva do ATR desde o breakout.

Uma extensão duration-aware deve estimar duração residual, porque um regime pode estar corretamente classificado mas perto do fim; este é precisamente um caso em que a confiança do estado não basta.[^10]

## Recuperação pós-choque

### Veto por engine

O Fast-Sentinel deve continuar global e imediato. Após o mínimo de duas barras, porém, os engines devem ter regras de rearmamento diferentes:

| Componente | Após cooldown mínimo | Regra proposta |
|---|---|---|
| Engine A — Mean Reversion | Continua bloqueado | Exigir \(|Z_{15}|<1,5\), \(|Z_{60}|<2,0\), declínio da volatilidade e 2–3 barras de evidência de consolidação |
| Engine B — Trend | Pode ser avaliado | Exigir ausência de novo choque, breakout ainda válido, OBI/OFI persistente, livro saudável e movimento residual suficiente |
| Novo choque | Bloqueio imediato | Zerar acumuladores e reiniciar cooldown |

Não se recomenda simplesmente alargar \(|Z_{60}|<2,0\) para “voltar ao mercado”. O arrasto de \(Z_{60}\) é intencional para Mean Reversion: depois de um salto, níveis O-U e bandas podem estar temporariamente mal especificados. A alteração correta é impedir que esse gate específico de Consolidação paralise automaticamente um engine de Trend que tem confirmação independente.

Cripto apresenta caudas pesadas e clustering de volatilidade; eventos extremos aparecem com muito mais frequência do que num modelo normal. Por isso, thresholds chamados “sigma” devem ser calibrados com resíduos Student-t, quantis empíricos ou EVT, e não interpretados como probabilidades gaussianas.[^11][^1]

## Universo e cluster lock

### Efeito na chegada

Se as oportunidades elegíveis de cada ativo fossem processos independentes com taxas \(\lambda_i\), a taxa agregada livre seria:

\[
\lambda_{total}=\sum_i \lambda_i
\]

Com sinais correlacionados, filtros comuns e um único slot, o ganho é menor. A taxa executada aproxima-se da taxa de chegada agregada multiplicada pela probabilidade de o lock estar livre; para durações médias \(E[H]\), a ocupação cresce aproximadamente com \(\lambda_{exec}E[H]\). Adicionar ativos aumenta oportunidades observadas, mas também colisões e oportunidades rejeitadas.

O Sharpe não aumenta mecanicamente. Pode subir se os novos ativos fornecerem edge líquido menos correlacionado, melhores seleções e menor variância do PnL por oportunidade; pode cair se acrescentarem falsos breakouts, adverse selection ou se um ativo ruidoso ganhar o lock antes de um sinal melhor. Estudos de trend-following cripto encontram benefícios de breadth, mas também mostram que custos, liquidez e correlações limitam o ganho marginal.[^12][^13]

Durante crashes, a diversificação aparente reduz-se: a dependência de cauda inferior entre criptoativos é muito mais forte e o mercado tende a cair como um bloco. O cluster lock reduz exposição simultânea, mas não torna sinais de ativos diferentes independentes nem elimina risco de seleção adversa no ativo escolhido.[^14]

### Arbitragem obrigatória

O desenho “primeiro a adquirir `hl_portfolio_cluster.lock`” deve ser substituído por:

1. Todos os ativos publicam candidatos para o mesmo `bar_close_id`.
2. O coordenador aguarda uma janela curta e determinística para recolher candidatos.
3. Cada candidato recebe um score comparável e líquido de custos.
4. O coordenador escolhe no máximo um.
5. Só então emite fencing token e lease ao engine vencedor.

Um score inicial pode combinar expectativa prevista, força HMM, persistência OBI, liquidez, distância ao stop, funding e penalização por correlação com o choque dominante. A prioridade nunca deve depender da ordem de scheduling dos processos.

## Seleção de ativos

### Snapshot atual

Uma amostra curta da API oficial, com 24 snapshots por ativo ao longo de cerca de dois minutos, encontrou a seguinte ordenação de microestrutura. É apenas um **sanity check corrente**, não uma medição histórica suficiente.

| Ativo | Spread mediano | Profundidade mediana ±10 bps | Volume 24h no snapshot | Leitura |
|---|---:|---:|---:|---|
| DOGE | 0,60 bps | cerca de 230 mil USD | cerca de 29,7 M USD | Melhor top-of-book, mas risco idiossincrático elevado |
| AVAX | 1,00 bps | cerca de 146 mil USD | cerca de 29,5 M USD | Melhor compromisso para piloto |
| SUI | 1,95 bps | cerca de 218 mil USD | cerca de 44,7 M USD | Boa profundidade, spread mais largo e risco de unlock |
| ARB | 2,08 bps | cerca de 51 mil USD | cerca de 13,5 M USD | Atrito e profundidade desfavoráveis |

A API `metaAndAssetCtxs` fornece mark, funding, open interest e volume nocional diário, mas estes valores são snapshots dinâmicos. A decisão precisa de medianas por hora, percentis de spread, profundidade a 5/10/25 bps, resiliência após agressões e fill quality durante pelo menos 30 dias.[^15][^16]

### Persistência recente

Nas aproximadamente 5.000 velas recentes, nenhum dos quatro candidatos demonstrou simultaneamente um O-U global convincente e breakouts Donchian limpos. A regressão O-U global sobre log-price é, de qualquer modo, um teste inadequado para o Engine A, que opera apenas dentro de regimes; a análise correta é condicional às barras classificadas como Consolidação.

As correlações recentes de retornos de 15 minutos com BTC/ETH/SOL foram mais baixas para ARB e mais altas para DOGE, mas a janela tem só cerca de 52 dias e inclui o choque corrente. A baixa correlação de ARB não compensa automaticamente a sua menor liquidez e o risco de tokenomics.

### Ranking recomendado

| Ranking | Ativo | Decisão | Fundamentação |
|---:|---|---|---|
| 1 | AVAX | Adicionar primeiro em shadow | Microestrutura corrente equilibrada, volume/OI relativamente ativo e menor risco meme |
| 2 | SUI | Segundo shadow; live apenas com veto de eventos | Maior volume e boa profundidade, mas spread mais largo e unlocks recorrentes |
| 3 | DOGE | Excluir do live; opcional em shadow segregado | Livro corrente forte, porém concentração de custódia, sensibilidade a narrativas e natureza meme |
| 4 | ARB | Excluir por agora | Livro mais fino, spread maior e overhang de unlock/governance |

DOGE tem forte concentração em endereços de topo, embora parte represente custódia de exchanges; por isso, a métrica não equivale diretamente a uma única baleia, mas confirma concentração operacional relevante. SUI tinha cerca de 41% da oferta em circulação em outubro de 2026 e um calendário prolongado de unlocks, exigindo veto de calendário e monitorização de depósitos em exchange. ARB permanece um token de governance com unlocks de equipa, investidores e Foundation até 2027, criando overhang de oferta e risco de decisões de tesouraria.[^17][^18][^19][^20]

“Exclusão categórica” deve significar **fora do live universe até cumprir critérios objetivos**, e não uma afirmação eterna sobre o ativo. Para DOGE, exigir estabilidade de spread/depth, ausência de evento narrativo e validação separada por regime; para ARB, exigir melhoria sustentada de liquidez e redução do overhang.

## Protocolo de validação

### Fases

**Fase 0A — congelar live, melhorar telemetria por 7 dias**

- Manter os parâmetros atuais no caminho live.
- Registar todos os gates e sinais contrafactuais.
- Acrescentar AVAX e SUI apenas ao recorder/shadow.
- Capturar L2/OBI continuamente, não só no fecho da vela.

**Fase 0B — replay de 90 dias mínimos**

- Baseline atual contra dwell assimétrico e acumulador.
- Reexecução causal, sem revisão de candles nem leakage.
- Custos maker, fill probability, queue position conservadora, funding e cancelamentos.

**Fase 0C — shadow simultâneo durante 30 dias**

- O baseline e cada challenger recebem o mesmo feed.
- Nenhum arm altera capital real.
- Registar conflito de lock e qual arm teria sido selecionado.

**Fase 1 — canary**

- Promover apenas se houver amostra suficiente de trades, estabilidade entre ativos/subperíodos e melhoria de cauda.
- Começar com risco reduzido e kill switches existentes.
- Nunca promover com base apenas no aumento de frequência.

### Critérios de promoção

- Expectancy líquida positiva com bootstrap por blocos.
- Expected Shortfall e máximo drawdown não piores do que o baseline dentro da margem definida.
- Menor atraso de Trend sem aumento material de falsos breakouts.
- Mean Reversion não entra durante normalização pós-choque.
- Resultado estável por ativo, mês, sessão UTC e lado long/short.
- Deflated Sharpe Ratio após contabilizar **todas** as variantes testadas; o DSR corrige seleção por múltiplos testes e retornos não normais.[^21][^22]
- Probability of Backtest Overfitting via CSCV; o método testa se o vencedor in-sample cai abaixo da mediana out-of-sample.[^23]

## Decisão operacional

Não alterar hoje os limiares live devido ao evento de -8,5 sigma. Alterar imediatamente a **observabilidade** e lançar challengers em shadow é, porém, cientificamente correto e evita confundir prudência com imobilismo.

A configuração challenger prioritária é:

- Trend: 1 barra se \(P_T\ge0,90\), 2 barras se \(P_T\ge0,82\), sempre com Donchian + OBI/OFI persistente + livro saudável + orçamento residual.
- Consolidação: 2 barras em confiança forte ou 3 barras equivalentes via acumulador.
- Leaky log-odds com meia-vida inicial de 2 barras para Trend e 3–4 para Consolidação.
- Hard shock reset preservado; cooldown mínimo de 2 barras preservado.
- Recuperação separada por engine, sem aliviar o gate O-U pós-choque.
- Universo shadow: BTC, ETH, SOL, AVAX e SUI.
- Universo live atual inalterado até a comparação produzir evidência.
- DOGE e ARB fora do live universe nesta fase.
- Coordenador portfolio-level com ranking determinístico antes de conceder o lock.

Este desenho mantém a propriedade mais valiosa demonstrada no primeiro dia — capacidade de ficar em caixa durante uma cascata — enquanto testa, de forma falsificável, se a latência de confirmação e o reset total estão a eliminar oportunidades economicamente válidas.

---

## References

1. [Tail risk of Bitcoin and major cryptocurrencies: a distributional ...](https://link.springer.com/article/10.1007/s44491-026-00008-7?error=cookies_not_supported&code=58c13d85-89ce-4319-b451-f5ea727947ba)

2. [Frontiers | Value at Risk long memory volatility models with heavy-tailed distributions for cryptocurrencies](https://www.frontiersin.org/journals/applied-mathematics-and-statistics/articles/10.3389/fams.2025.1567626/full) - This paper investigates the volatility dynamics and underlying long memory features of four major cr...

3. [Turn-of-the-candle effect in bitcoin returns - PMC](https://pmc.ncbi.nlm.nih.gov/articles/PMC10015199/) - This study discovers a statistically and economically significant intraday anomaly on Bitcoin market...

4. [[2109.12142] Periodicity in Cryptocurrency Volatility and LiquidityWe ...](https://ar5iv.labs.arxiv.org/html/2109.12142) - We study recurrent patterns in volatility and volume for major cryptocurrencies, Bitcoin and Ether, ...

5. [Bitcoin intraday time series momentum - Shen - Wiley Online Library](https://onlinelibrary.wiley.com/doi/10.1111/fire.12290)

6. [Historical data | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data)

7. [Info endpoint | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)

8. [A Comparative Analysis Of Regime-Switching Models In ...](https://www.politesi.polimi.it/retrieve/a4fb0146-1c24-43c2-bc13-dc26f729a2a8/2025_12_Jia_executive_summary_02.pdf)

9. [[PDF] CREATES Research Paper 2010-51 A Duration Hidden Markov ...](https://pure.au.dk/ws/files/21775779/rp10_51.pdf)

10. [Hidden Semi-Markov Models for Duration-Aware Regime Detection ...](https://www.mql5.com/en/articles/24460) - Standard HMMs assume geometric, memoryless state durations, which poorly match real market phases. T...

11. [Stylized Facts of High-Frequency Bitcoin Time Series - arXiv](https://arxiv.org/html/2402.11930v2)

12. [In Crypto We Trend - Man Group](https://www.man.com/insights/in-crypto-we-trend) - Cryptocurrencies may be well suited to trend-following approaches. However, the quantity of coins tr...

13. [Concretum Research](https://concretumgroup.com/wp-content/uploads/2026/02/Catching-Crypto-Trends.pdf)

14. [Crashing Together, Rallying Apart: Dynamic Conditional Tail Dependence in Cryptocurrency Markets](https://arxiv.org/html/2606.16840)

15. [metaAndAssetCtxs | Hyperliquid info - Chainstack Docs](https://docs.chainstack.com/reference/hyperliquid-info-meta-and-asset-ctxs)

16. [Perpetuals | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals)

17. [SUI/USD Live Price Chart, Market Cap & News Today](https://www.coingecko.com/en/coins/sui) - Price of Sui (SUI) today is $1.00 with a 24-hour trading volume of $839,973,370. Track Sui's live pr...

18. [SUI Token Unlock Schedule & Vesting - Tokenomics.com](https://app.tokenomics.com/tokenomics/sui/unlocks) - Sui (SUI ) token unlock schedule and vesting timeline. Track upcoming unlock dates, cliff releases, ...

19. [Dogecoin by the Numbers: 156 Billion Coins, 10,000 More Every Minute, and ETFs That Barely Hold Any](https://sqmagazine.co.uk/dogecoin-statistics/) - Supply trackers disagree by about 15.7 billion DOGE, new coins arrive at a fixed 10,000 per block, a...

20. [Arbitrum Token Transparency Filing - Blockworks](https://blockworks.com/token-transparency/filing/arbitrum) - Blockworks is a data and software platform for crypto markets. It offers investor relations software...

21. [The Deflated Sharpe Ratio: Correcting for Selection Bias, Backtest Overfitting and Non-Normality](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=2460551) - With the advent in recent years of large financial data sets, machine learning and high-performance ...

22. [The Deflated Sharpe Ratio: Correcting for Selection Bias, Backtest
               
               Overfitting, and Non-Normality](https://www.pm-research.com/content/iijpormgmt:::40:::5:::94.full.pdf?implicit-login=true&sigma-token=-qe3q8GbUspexC6f_33E65AeFQlY-T4alKKaEuOGB_Q)

23. [THE PROBABILITY OF BACKTEST OVERFITTING](https://www.davidhbailey.com/dhbpapers/backtest-prob.pdf)

