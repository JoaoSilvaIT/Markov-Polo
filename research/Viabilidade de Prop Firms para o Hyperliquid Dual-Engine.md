# Viabilidade de Prop Firms para o Hyperliquid Dual-Engine

**Data de corte:** 10 de outubro de 2026  
**Destinatários:** Researcher 1 — Quant Strategy & Microstructure; Researcher 2 — Risk, Legal & Tax Compliance  
**Âmbito:** avaliação técnica, económica, contratual e fiscal para um residente fiscal em Portugal. Este parecer não substitui consulta a advogado, contabilista certificado nem informação vinculativa da Autoridade Tributária (AT).

## Executive Summary & Veredito

### Veredito: **HYBRID, com Hyperliquid como produção principal**

A recomendação é manter o sistema original e o capital próprio na Hyperliquid como **motor económico principal** e admitir apenas um **piloto isolado de prop firm crypto-native**, sem recalibrar agressivamente a estratégia para “passar o challenge”. O piloto deve começar em shadow mode, avançar para a avaliação mais barata compatível e só escalar depois de pelo menos um payout efetivamente recebido e reconciliado.

O motivo central não é apenas o drawdown. O Engine A depende de características que fazem parte da própria alpha: livro L2 real, ordens post-only e economics de maker. A Hyperliquid expõe L2 por WebSocket, suporta ALO/post-only — que cancela em vez de cruzar o spread — e paga rebates maker conforme o tier; isto não é reproduzido por CFDs sintéticos de MT5/cTrader nem por terminais que apenas imitam um feed de exchange. Na FTMO, por exemplo, os cripto-CFDs passaram a cobrar 0,0325% por lado, isto é, 6,5 bps round-trip só de comissão, antes do spread e slippage; esse piso já consome 26% de uma borda bruta mínima de 25 bps.[^1][^2][^3][^4][^5][^6]

A transição mais defensável é, portanto:

- **Engine A:** No-Go em prop firms CFD; Go condicional apenas numa integração com order book real e suporte explícito a API/bots e limit/post-only.
- **Engine B:** tecnicamente portável para crypto-native perps e, com reestimação total de custos/slippage, também para alguns ambientes CFD; ainda assim, exige um adapter de execução e um risk gateway específicos.
- **HMM e Shock Veto:** manter a lógica de regime; adicionar uma camada contratual que impeça qualquer cancelamento, stop ou fecho numa janela de notícias quando a regra da firma proíba essas ações.
- **Capital próprio:** continuar como referência de produção e de verdade económica. Não tratar uma conta de “$100k” como $100k de capital económico: numa avaliação com 5% de perda máxima, o orçamento real de risco é aproximadamente $5k, e a firma pode continuar a operar apenas capital simulado.

Também é necessário corrigir uma premissa fiscal do briefing: **perpetual futures não beneficiam automaticamente da exclusão de 365 dias aplicável à alienação de criptoativos spot que não sejam valores mobiliários**. O CIRS enquadra operações sobre instrumentos financeiros derivados na Categoria G e tributa o saldo positivo, em regra, a 28%; a documentação do Anexo J inclui futuros, opções, forwards e swaps no código de derivados. A regra dos 365 dias continua relevante para criptoativo spot elegível, mas não transforma ganhos de Hyperliquid perps mantidos por um ano em rendimento isento.[^7][^8][^9][^10][^11][^12]

## Limite Quantitativo Essencial

Não existe base estatística suficiente no briefing para calcular uma **probabilidade de aprovação** ou um **expected time-to-pass** defensável. Faltam, no mínimo, uma série cronológica de PnL líquido, MAE/MFE, exposição intradiária, sequência de trades, fees/rebates, slippage e timestamps; qualquer percentagem ou número de dias produzido sem esses dados seria inventado.

A métrica correta é um problema de first passage com duas barreiras absorventes: aprovação ao atingir o target e falha ao tocar o daily drawdown ou max drawdown. Para cada trajetória reamostrada, deve calcular-se:

\[
\tau = \inf\{t: E_t \ge E_0(1+T)\;\lor\;E_t \le B_t^{max}\;\lor\;E_t \le B_t^{daily}\}
\]

onde \(T\) é o profit target, \(B_t^{max}\) reproduz exatamente a regra static/trailing/high-water-mark da firma e \(B_t^{daily}\) inclui PnL realizado, floating PnL, fees, funding e reset de sessão. Os outputs necessários são \(P(pass)\), mediana e percentis 25/75 de \(\tau\) condicionados a aprovação, probabilidade de breach diário, probabilidade de breach total e custo esperado de tentativas.

Uma aproximação determinística como target dividido pelo retorno médio diário é inadequada: ignora autocorrelação de regimes, dias sem trades, path dependency do trailing drawdown e o facto de a barreira poder mover-se com o high-water mark. A Breakout, por exemplo, mede equity incluindo posições abertas, aplica simultaneamente limites diário e total e, no programa de duas fases, faz o drawdown total acompanhar o maior saldo; tocar a barreira por qualquer instante causa breach.[^13][^14]

### Protocolo de estimação exigido

1. Reexecutar pelo menos 12 meses de eventos L2 e fills reais, preservando períodos de 48–72 horas sem posições.
2. Substituir o modelo de custos Hyperliquid pelo fee schedule, spread, latência e slippage de cada candidato.
3. Aplicar block bootstrap por blocos de regime HMM, nunca shuffle IID de trades individuais.
4. Reproduzir timezone, reset diário, floating PnL, funding, consistency/best-day rules e inatividade.
5. Produzir intervalos de confiança com reamostragem e walk-forward; não escolher parâmetros usando o mesmo período que mede a aprovação.
6. Exigir validação shadow-live no venue candidato antes de pagar um challenge.

Sem esta análise, a decisão correta é **não estimar**. A baixa trade-arrival rate não é, por si, incompatível com programas sem limite temporal: HyroTrader fecha por inatividade apenas após 30 dias, enquanto Breakout anuncia ausência de prazo máximo e de minimum trading days em certos programas. Três dias em cash não constituem problema nestas regras; 30 dias sem trade já constituem.[^15][^16]

## Recalibração para Challenge

Não se recomenda multiplicar risco, reduzir thresholds ou forçar trades para atingir 8–10%. Isso altera simultaneamente a distribuição de retornos, a seleção de regimes e os custos de execução; deixa de ser a estratégia validada e cria overfitting à função de payout da prop firm.

A adaptação aceitável é **risk-budgeting**, não signal mining:

- Dimensionar cada ordem por distância até à barreira mais próxima, incluindo floating PnL e fees.
- Reservar 40–50% do limite diário como buffer operacional para gaps, slippage, funding e divergência entre relógios.
- Usar o mesmo sinal e os mesmos parâmetros de regime; apenas reduzir size quando a folga contratual diminui.
- Se a probabilidade de aprovação com parâmetros congelados for economicamente baixa, rejeitar o programa em vez de “otimizar para passar”.

## Matriz de Compatibilidade Técnica

| Regra/feature | Engine A — Mean Reversion | Engine B — Breakout | HMM / Shock Controller | Decisão |
|---|---|---|---|---|
| Target 8–12% | Cadência baixa pode alongar muito o prazo; forçar frequência destrói seleção | Mais compatível em regimes trend, mas concentrado e sujeito a best-day rules | Dwell e Neutral preservam robustez, embora atrasem target | Só programas sem prazo rígido |
| Daily equity drawdown | Floating MAE pode causar breach antes do bounce | Breakout falso + slippage pode tocar limite instantaneamente | Precisa de estado “distance-to-breach” em tempo real | Hard cap interno muito abaixo da regra |
| Trailing/high-water drawdown | Muito adverso: ganhos iniciais apertam a margem para reversão | Adverso a sequência win-then-pullback | O HMM não conhece hoje a barreira contratual | Preferir static drawdown |
| Max total drawdown | Compatível se size permanecer conservador | Compatível apenas com stop e slippage bounds | Cluster lock 0/1 reduz correlação simultânea | Favorável, mas não suficiente |
| Inatividade | 48–72h é aceitável; 30 dias pode não ser | Idem | Não criar “heartbeat trade” artificial | Monitorizar prazo; nunca operar só para cumprir atividade |
| Minimum trading days | Pode cumprir naturalmente, mas lentamente | Normalmente mais fácil | Não fragmentar posições para fabricar dias | Evitar se exigir tamanho mínimo por dia |
| Best-day/consistency | Poucos winners podem exceder o limite | Risco alto de um único breakout dominar resultado | Precisa de forecast da regra antes de cada entrada | Preferir programa sem consistency rule |
| Execução post-only | Essencial; impossível em CFD convencional | Não essencial, mas limit/stop semantics mudam | Necessita venue capability map | Engine A só em exchange-like API |
| L2 OBI | Feed do CFD não equivale ao venue de execução | OBI externo cria basis/latency risk | Features devem ser calculadas no venue executado | Recalibrar com L2 do próprio venue |
| Fees/spread | Edge de 25 bps pode desaparecer | Suporta mais atrito se payoff for largo | Custos entram antes do veto/entry | Rejeitar se edge líquido ficar abaixo do mínimo |
| News restriction | Cancel/exit automático pode violar regra | Stop/entry durante janela pode violar | Shock Veto deve obedecer a blackout calendar | Preferir contas sem news restriction |
| EA/API | Requer REST/WebSocket e client-order IDs | Portável por API ou EA | Necessita kill switch auditável | Contrato escrito, não FAQ informal |
| HFT/tick scalping ban | Cancel/replace excessivo pode parecer HFT | Menor risco | Introduzir rate governor e minimum resting time | Logar todas as decisões e throttling |
| IP/VPS/copy trading | VPS dedicado e estratégia própria são defensáveis | Idem | Identidade, IP e ownership devem ser estáveis | Sem VPS partilhado nem trade copier |
| 24/7 e funding | Nativo em perps | Nativo em perps | Funding deve entrar no equity risk | Preferir crypto-native 24/7 |

A FTMO permite EAs/algoritmos, mas proíbe latency arbitrage, práticas incompatíveis com mercados reais e hiperatividade superior a 2.000 pedidos de servidor por dia; contas Standard financiadas também proíbem abrir ou fechar — inclusive por SL/TP — entre dois minutos antes e dois minutos depois de certos anúncios, enquanto Swing não tem essa restrição. Logo, o Shock Veto não é automaticamente conforme: um fecho disparado no blackout pode ser breach, embora tenha sido concebido para reduzir risco.[^17][^18][^19]

## Microestrutura e Sobrevivência do Engine A

### Hyperliquid vs. CFD

A Hyperliquid implementa um order book on-chain com prioridade preço-tempo, endpoint público de ordens, ALO/post-only e streams L2; maker rebates são pagos por fill e podem ser negativos conforme o tier. Esses elementos determinam fill probability, queue position, adverse selection e o custo líquido do Engine A.[^3][^4][^20][^6]

Num CFD, “limit order” não implica participação num livro central nem rebate maker. O broker/prop pode aplicar spread, comissão, markup, bridge latency e regras de execução próprias. Na FTMO, a comissão cripto publicada de 0,0325% por lado equivale a 3,25 bps por lado e 6,5 bps round-trip antes de spread/slippage. Portanto, mesmo que o spread observado não seja 30–80 bps, não se deve assumir equivalência económica com Hyperliquid sem recolher ticks bid/ask e fills reais do símbolo e plataforma específicos.[^21][^1]

### Conclusão por engine

**Engine A em CFD MT5/cTrader: No-Go.** Não basta recalibrar \(\sigma\). Seria necessário redesenhar o modelo de fill, substituir OBI por dados do broker, aumentar a borda líquida e aceitar execução taker/limit sem maker economics. Isso equivale a uma nova estratégia que exige investigação própria.

**Engine A numa prop crypto-native: Go condicional.** Só é elegível se a API permitir bot próprio, limit/post-only, market data do mesmo venue, fills e fees verificáveis, e se os T&C aceitarem a frequência de cancel/replace. Mesmo assim, Bybit demo é mercado simulado com dados reais; não é prova de queue position ou impacto idênticos ao live.

**Engine B: portabilidade média/alta.** Donchian e Chandelier sobrevivem melhor ao venue change, mas o OBI deve vir do livro onde a ordem é executada. Se for usado L2 externo de Bybit para executar num CFD, cria-se risco de sinal lead/lag que pode ser interpretado como feed exploitation.

## Benchmark de Prop Firms

As contas abaixo continuam a ser predominantemente **simuladas**. O valor económico é um direito contratual a reward/payout, não propriedade sobre o saldo nominal. HyroTrader declara expressamente que não existe fase de capital real; FTMO também declara que o trader opera capital fictício e recebe uma recompensa real baseada na performance simulada.[^22][^23][^24]

| Firma | Execução / automação | Regras relevantes | Fit | Veredito |
|---|---|---|---|---|
| **HyroTrader** | Bybit ou BloFin demo ligado por API; dados/order book da exchange; material oficial aceita custom bot, vedando abuso/HFT[^25][^26] | Sem prazo; inatividade após 30 dias; opções 1/2-step; daily/max loss dependem do plano; consistency de 40% na avaliação em alguns planos[^15][^27][^28] | Melhor candidato para A e B, sujeito a prova de post-only e semantics de fills | **Piloto recomendado** |
| **Crypto Fund Trader** | Bybit pessoal/subaccount por API para USDT futures; FAQ proíbe HFT e interferência na ligação[^29][^30] | Targets variam por programa; capital e performance são demo; regras devem ser congeladas por versão[^31][^32] | Potencialmente compatível, mas a documentação pública é ambígua sobre se a mesma key pode ser usada livremente pelo bot em todas as fases | **Due diligence / alternativa** |
| **Breakout, backed by Kraken** | Crypto-native, 24/7, DXtrade e pricing/liquidez de exchange; o acordo adiciona 500 ms para prevenir latency arbitrage[^13][^33][^34] | 0,04% por lado, 5x BTC/ETH, 2x restantes; daily 3% ou 5%; drawdown static ou trailing conforme plano[^13][^14] | Engine B pode ser testado; Engine A sofre com fee de 8 bps round-trip e latência artificial; não foi identificada API pública adequada ao bot | **B-only / observação** |

### Due diligence antes de pagar

- Obter por email a confirmação de que Python/Kotlin bot, REST/WebSocket, limit post-only, cancel/replace e execução 24/7 são permitidos **na avaliação e na fase funded**.
- Guardar PDF/hash dos T&C, fee schedule e rulebook vigentes no dia da compra.
- Identificar entidade contratante, jurisdição, endereço, mecanismo de litígio, KYC, payout currency e cláusulas discricionárias.
- Testar se “real market data” significa apenas preços ou também matching/queue/fills realistas.
- Pedir exemplos formais de daily drawdown: timezone, floating PnL, commissions, funding, depósitos/payouts e high-water mark.
- Confirmar elegibilidade de residente em Portugal e do venue terceiro; HyroTrader identifica uma entidade operacional nos EAU, funded accounts nas Ilhas Virgens Britânicas e processamento de cartões EEE por entidade eslovaca. Essa cadeia aumenta o risco de contraparte e a importância dos documentos.[^35]

FTMO não é recomendada para este sistema: oferece cripto-CFDs, não o mesmo order book, cobra comissão percentual por lado e impõe, em algumas contas, news blackout incompatível com exits automáticos. Pode servir como controlo experimental para uma versão nova do Engine B, não como port direto do sistema atual.[^17][^1]

## Sentinel Shock Veto e Notícias

O Sentinel tem duas ações distintas e devem ser separadas no código:

- **Veto de novas entradas:** normalmente compatível e desejável; durante blackout deve simplesmente bloquear ordens novas.
- **Saída/cancelamento:** pode violar regras que proíbam qualquer open/close ou execução de SL/TP na janela. A FTMO declara expressamente que até SL ou TP disparado no intervalo de ±2 minutos conta como breach numa Standard FTMO Account.[^17]

A ordem de prioridade operacional deve ser: segurança de mercado e limite de perda legalmente assumido primeiro; porém, se a firma proíbe o exit, a solução não é ignorar o risco — é não manter exposição que possa precisar de sair nessa janela. O adapter deve iniciar um **pre-news flatten** antes do blackout, cancelar ordens resting e depois entrar em `CONTRACTUAL_BLACKOUT`, durante o qual nenhum comando é enviado. Se a firma não permitir esta arquitetura ou se os anúncios relevantes para cripto não forem definidos objetivamente, rejeitar o programa.

## Enquadramento Legal Portugal

### É legal contratar uma prop firm internacional?

Não foi identificada uma proibição geral que impeça um residente português de celebrar um contrato de prestação de serviços com uma empresa estrangeira para produzir sinais ou resultados em ambiente simulado. O risco muda se a entidade estiver, na substância, a oferecer ao residente um serviço de investimento, CFDs/perpetuals reais ou a receber fundos do cliente; nesse caso, devem ser verificadas autorizações e passaporte no registo da CMVM/ESMA. A CMVM disponibiliza listas de entidades autorizadas e alertas sobre não autorizadas.[^36][^37][^38]

A ESMA considera que derivados comercializados como perpetual futures/perpetual contracts podem cair no perímetro das medidas de CFDs quando preencham a respetiva definição; essas medidas incluem leverage caps, margin close-out, negative balance protection e risk warnings. Para CFDs de cripto vendidos a investidores de retalho, a referência europeia é 2:1 de alavancagem. Uma avaliação puramente simulada pode ficar fora desse perímetro, mas o rótulo “funded” não decide a qualificação jurídica: contrato, fluxos de dinheiro, execução e contraparte decidem.[^39][^40][^41]

### AML, bancos e payout

Não há garantia de que CGD, Millennium BCP, Novo Banco ou ActivoBank aceitem automaticamente cada fluxo, nem evidência para prever qual banco bloqueará uma transferência específica. As instituições devem avaliar origem/destino e podem exigir documentação adequada ao risco; o Banco de Portugal prevê comprovação da origem de fundos e, para ativos virtuais, histórico de wallets, hashes e comprovativos de aquisição/transação.[^42][^43]

Manter um dossier por payout:

- contrato e funded-trader agreement;
- invoice/fatura-recibo emitida;
- dashboard e statement com período e cálculo do reward;
- email de aprovação do payout;
- TXID, endereço originador/destinatário e screenshot do explorador;
- KYC da firma e identificação da entidade pagadora;
- conversão EUR na data de recebimento e extrato da exchange/off-ramp;
- prova da declaração fiscal e da origem do capital próprio.

Preferir transferência SEPA em nome da entidade contratante quando disponível. Se o payout for USDT/USDC, receber numa wallet dedicada à atividade e converter por venue regulado/KYC, evitando misturar fundos pessoais e empresariais.

## Parecer Fiscal Portugal

### Payout de prop firm

A caracterização mais defensável é **Categoria B — prestação de serviços/remuneração contratual por performance**, e não Categoria G sobre cripto, porque o trader não aliena um ativo próprio nem detém o saldo simulado. A própria Breakout qualifica o funded trader como independent contractor, e FTMO descreve o pagamento como reward real por trading simulado. A AT exige fatura, recibo ou fatura-recibo por todas as importâncias recebidas em prestações de serviços.[^44][^23][^45][^46]

Contudo, “performance fee”, prémio, cessão de dados/sinais e prestação técnica podem receber qualificações distintas conforme o contrato. Antes do primeiro payout material, deve ser apresentado um pedido de informação vinculativa à AT com o contrato completo, fluxos e moeda de pagamento.

### Abertura de atividade

1. Abrir atividade antes do primeiro serviço/payout previsível.
2. Descrever a atividade real, por exemplo “prestação de serviços de análise e execução simulada/fornecimento de dados e estratégias a empresa estrangeira”.
3. Validar o código com contabilista e, se necessário, INE/AT. Para pessoa singular, **CIRS 1519 — Outros prestadores de serviços** é um candidato residual, não uma resposta automática; a AT confirma que 1519 só deve ser usado quando nenhum CAE/CIRS mais específico se aplica.[^47][^48][^49]
4. Registar operações intracomunitárias se houver clientes B2B na UE e validar o VAT ID no VIES.
5. Emitir fatura ou fatura-recibo em EUR pelo valor do payout na data fiscal relevante; se recebido em stablecoin, guardar fonte e timestamp da conversão.

No regime simplificado, o coeficiente fiscal exato depende da qualificação efetiva da atividade. A jurisprudência e doutrina distinguem atividades especificamente previstas, frequentemente com coeficiente 0,75, de serviços residuais que podem usar 0,35; escolher 1519 apenas para obter coeficiente menor é vulnerável a requalificação. Por isso, o modelo económico deve correr ambos os cenários até existir posição escrita.[^50][^51]

### Segurança Social

Para trabalhadores independentes, a taxa contributiva é 21,4%; em prestações de serviços, o rendimento relevante corresponde, em regra, a 70% das receitas do trimestre, e a base mensal é um terço desse rendimento relevante. A carga económica padrão aproximada, antes de ajustes, limites e isenções pessoais, é portanto 21,4% × 70% = 14,98% da receita de serviços. Deve verificar-se eventual isenção inicial, acumulação com trabalho por conta de outrem e limites vigentes no caso concreto.[^52][^53][^54]

### IVA — UE e fora da UE

Para B2B, a regra geral localiza o serviço no país do adquirente. Se a prop firm for sujeito passivo noutro Estado-Membro, a fatura é emitida sem IVA português com a menção aplicável de autoliquidação/reverse charge, e a operação entra na declaração periódica e na declaração recapitulativa; a AT exige a recapitulativa nos períodos em que esses serviços intracomunitários forem realizados.[^55][^45][^56]

Se o cliente empresarial estiver fora da UE, o serviço B2B, em regra, também não é localizado em Portugal; reporta-se no campo aplicável da declaração periódica, mas não na recapitulativa VIES. É essencial verificar que o contratante é efetivamente sujeito passivo e guardar prova da sede e atividade; se for consumidor/não sujeito passivo, a conclusão pode mudar.[^57][^55]

### Payout recebido em USDT/USDC

O rendimento de serviços deve ser reconhecido pelo valor em euros quando recebido ou colocado à disposição, conforme o regime aplicável. A partir daí, os tokens passam a ter um custo fiscal documentado; valorização ou desvalorização posterior até à alienação constitui evento separado. A AT já decidiu, em contexto de serviços pagos em criptomoeda, que a remuneração mantém natureza de Categoria B e exige emissão de fatura/recibo.[^58]

### Capital próprio na Hyperliquid

O briefing assume 28% ou 0% após 365 dias, mas isso só é seguro para alienação de criptoativo elegível que não seja valor mobiliário e não esteja noutra categoria. Perpetuals são economicamente derivados; o CIRS inclui operações relativas a instrumentos financeiros derivados na Categoria G e o saldo positivo é sujeito, em regra, a 28%. Não existe uma isenção de 365 dias para manter um perp aberto.[^8][^10][^11][^7]

Além disso, trading algorítmico contínuo, organizado e profissional pode levantar discussão de Categoria B. A fronteira depende de habitualidade, organização, meios e natureza concreta da atividade. Recomenda-se pedido de informação vinculativa separado para: (a) perps Hyperliquid em nome individual; (b) payout de prop; (c) recebimento em stablecoin.

## Sociedade Unipessoal Lda

Não existe um patamar universal de **receita** a partir do qual uma Lda “compensa”. A variável relevante é lucro tributável, montante retido na sociedade, despesas dedutíveis, remuneração do gerente, dividendos, Segurança Social, derrama municipal, contabilidade e risco jurídico.

Em 2026, a taxa geral de IRC no continente é 19%; PME/Small Mid Cap elegíveis pagam 15% sobre os primeiros €50.000 de matéria coletável e 19% sobre o excedente. Se todo o lucro pós-IRC for distribuído a uma pessoa singular e o dividendo for tributado a 28%, a carga combinada simplificada é aproximadamente 38,8% no bloco sujeito a 15% e 41,68% no bloco sujeito a 19%, antes de derramas e sem considerar englobamento. Assim, a vantagem da sociedade cresce sobretudo quando parte relevante do lucro **fica retida para reinvestimento**; se tudo for levantado, a dupla camada reduz o benefício.[^59][^60][^61]

Em vez de um falso threshold, executar simulações certificadas para receitas anuais de €30k, €50k, €75k e €100k sob:

- ENI simplificado com coeficiente 0,35 e 0,75;
- ENI com contabilidade organizada;
- Lda com salário mínimo/mercado do gerente e retenção de 0%, 25%, 50% e 75% do lucro;
- Lda com dividendos versus reinvestimento em infraestrutura/atividade.

**Trigger de revisão, não regra fiscal:** quando os payouts recorrentes se aproximarem de €50k/ano, ou quando for possível reter mais de 25–30% do lucro por vários anos, obter uma comparação formal de contabilista. A Lda também pode ser justificável mais cedo por segregação contratual, responsabilidade e documentação bancária, mesmo sem poupança fiscal.

## Veredito Económico em 12 Meses

Uma comparação numérica de Net Expected Return não é possível sem distribuição real de retornos, capital próprio inicial, fee de challenge, preço do plano, profit split, taxa de aprovação, número esperado de tentativas e custo fiscal pessoal. A formulação correta é:

\[
EV_{prop}=P_{pass}\times P_{survive}\times s\times E[PnL_{funded}] - E[fees\ de\ tentativas] - custos\ de\ adaptação - impostos
\]

\[
EV_{own}=E[PnL_{HL}] - fees - funding - perdas\ esperadas\ de\ venue/custódia - impostos
\]

onde \(s\) é o profit split. O saldo nominal de $100k não deve entrar diretamente como capital: o verdadeiro risk capital é delimitado pelo drawdown. Por exemplo, a Breakout combina contas simuladas com 3–5% de limite diário e limites totais definidos pelo plano; a HyroTrader declara que o saldo é simulado e o payout é uma quota contratual da performance.[^15][^13]

### Ranking estrutural

1. **Capital próprio Hyperliquid:** maior fidelidade à edge, controlo total, sem target temporal e sem risco de recusa discricionária de payout; em contrapartida, há risco real de perda, liquidação, protocolo, oracle, stablecoin e wallet.
2. **Prop crypto-native como opção:** limita perda pessoal à fee e pode multiplicar o notional económico do payout; adiciona risco de challenge, contraparte, alteração unilateral de regras, tributação como serviço e basis entre sim/live.
3. **Prop CFD tradicional:** pior combinação para o Engine A; a aparente escala nominal não compensa a destruição da microestrutura.

A decisão robusta em 12 meses é manter o autocompounding como core e tratar uma prop crypto-native como **opção de convexidade de risco limitado**, com budget anual fixo para fees e proibição de alterar os sinais para recuperar tentativas perdidas.

## Recomendações de Engenharia

### Arquitetura de adapters

Criar uma interface `VenueAdapter` comum:

```text
VenueCapabilities
  has_native_l2
  supports_post_only
  supports_client_order_id
  supports_reduce_only
  supports_atomic_cancel
  supports_trailing_stop
  api_rate_limits
  fee_model
  funding_model
  session_timezone
```

Nunca reutilizar implicitamente semantics da Hyperliquid. Cada adapter deve implementar normalização de lot/tick, order lifecycle, partial fills, cancel races, timestamp drift e reconciliação de posição.

### Risk gateway de prop

Adicionar um serviço independente antes do OMS:

```text
PropRiskState
  start_balance
  current_balance
  current_equity
  day_start_balance_or_equity
  high_water_mark
  daily_floor
  total_floor
  distance_to_daily_breach
  distance_to_total_breach
  best_day_ratio
  inactivity_days
  blackout_state
```

O gateway deve rejeitar qualquer ordem cujo stress PnL, incluindo fees e slippage, atravesse um **internal floor** mais conservador que a barreira oficial. Não confiar apenas no dashboard da firma, que pode atualizar em frequência ou timezone diferente.

### Alterações por engine

**Engine A**

- Ativar apenas se `has_native_l2 && supports_post_only`.
- Calcular OBI exclusivamente no venue de execução.
- Reestimar fill probability e adverse selection com fills demo observados.
- Introduzir cancel/replace governor e minimum resting interval para evitar classificação HFT.
- Desativar se a soma esperada de spread crossing, comissão, slippage e funding reduzir a edge líquida abaixo de 25 bps.

**Engine B**

- Recalibrar Donchian apenas para diferenças de feed/bar construction, sem otimizar ao target da firma.
- Implementar slippage-aware stop e stop-market fallback.
- Integrar funding e mark/index divergence no Chandelier.
- Bloquear entrada se o projected best-day contribution violar consistency rules.

**Controller**

- Adicionar estados `PRE_NEWS_FLATTEN`, `CONTRACTUAL_BLACKOUT`, `PROP_DD_QUARANTINE` e `INACTIVITY_WARNING`.
- Manter Shock Veto de mercado, mas separar `veto_entry` de `force_exit`.
- A quarentena de 30 minutos deve ser estendida até terminar o blackout contratual quando este for maior.

### Cluster lock e isolamento

O `hl_portfolio_cluster.lock` não deve ser partilhado diretamente com a prop. Criar locks separados por entidade de risco e um **Global Strategy Intent Bus** que impeça exposição agregada acidentalmente correlacionada. Não copiar ordens entre contas; emitir intenções independentes e deixar cada risk gateway decidir.

### Observabilidade e prova

Registar de forma imutável:

- market-data sequence e checksums;
- sinal, regime e motivo de cada ordem;
- snapshots das barreiras antes/depois;
- ordens, cancels, rejects, partial fills e latency;
- calendário de notícias e decisões de blackout;
- versões de código, parâmetros e rulebook;
- daily statement e reconciliação com a firma.

Estes logs servem para debugging, disputa de payout, auditoria fiscal e demonstração de que a estratégia é própria e não um copier.

## Plano de Implementação

### Fase 0 — Jurídico e fiscal

- Rever o funded-trader agreement da entidade escolhida.
- Confirmar API/bot por escrito.
- Abrir atividade e validar CIRS/CAE, IVA e moeda de faturação.
- Preparar dossier AML e wallet segregada.
- Submeter informação vinculativa à AT se os montantes forem materiais.

### Fase 1 — Shadow

- Ligar Bybit/BloFin demo ou feed candidato sem enviar ordens.
- Executar signals A/B em paralelo com Hyperliquid durante 60–90 dias.
- Medir divergência de OBI, bars, spreads, fills teóricos, fees e funding.
- Rejeitar o venue se Engine A perder a edge líquida.

### Fase 2 — Replay de regras

- Aplicar o rulebook exato à série shadow.
- Estimar first-passage por block bootstrap de regimes.
- Definir budget máximo de fee e número máximo de tentativas.
- Aprovar apenas se o limite inferior do intervalo de confiança do EV for positivo depois de impostos e adaptação.

### Fase 3 — Challenge mínimo

- Comprar apenas o menor plano economicamente informativo.
- Congelar parâmetros de sinal.
- Usar risk multiplier inferior ao da produção até validar semantics.
- Parar imediatamente se houver divergência de dashboard, fills ou regra não documentada.

### Fase 4 — Funded e payout

- Solicitar o primeiro payout cedo, em vez de acumular crédito de contraparte.
- Emitir a fatura/recibo e arquivar TXID/transferência.
- Só depois escalar uma dimensão; nunca comprar várias contas antes da prova de payout.

## Decisão Final

**Hybrid — Go apenas para um piloto crypto-native segregado; No-Go para portar o Engine A para prop firms CFD; No-Go para recalibração agressiva de challenge.** O sistema Hyperliquid deve permanecer o benchmark e core produtivo. HyroTrader é o candidato inicial mais alinhado com API e microestrutura; Crypto Fund Trader exige esclarecimento contratual adicional; Breakout é mais adequado a um teste do Engine B do que ao Engine A.

Antes de qualquer decisão quantitativa, a equipa deve fornecer a equity curve e os eventos de execução para calcular \(P(pass)\), time-to-pass e Net Expected Return. Antes do primeiro payout, deve ainda obter validação fiscal do enquadramento em Categoria B e corrigir o modelo comparativo para tratar Hyperliquid perps como derivados, sem presumir a isenção de 365 dias.

---

## References

1. [FTMO aprimora o trading de criptomoedas: Novos instrumentos e ...](https://ftmo.com/pt/blog/ftmo-aprimora-o-trading-de-criptomoedas-novos-instrumentos-e-spreads-mais-vantajosos/) - FTMO expande sua oferta cripto com 22 novos pares e melhorias nas condições. Veja o que muda para os...

2. [FTMO Enhances Crypto Trading: New Instruments and Better Spreads](https://ftmo.com/en/blog/ftmo-enhances-crypto-trading-new-instruments-and-better-spreads/) - FTMO expands crypto offering with 22 new pairs, better spreads, and major updates for active traders...

3. [Exchange endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint) - The exchange endpoint is used to interact with and trade on the Hyperliquid chain. See the Python SD...

4. [Fees | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees) - Order types Take profit and stop loss orders (TP/SL) Fees are based on your rolling 14 day volume … ...

5. [Info endpoint | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint) - The info endpoint is used to fetch information about the exchange and specific users. Retrieve a use...

6. [Subscriptions - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions) - This page describes subscribing to data streams using the WebSocket API. Exchange endpoint Websocket...

7. [irs10](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/cirs_rep/Pages/irs10.aspx)

8. [info.portaldasfinancas.gov.pt › pt › apoioCriptoativos - Conceito fiscal e tributação](https://info.portaldasfinancas.gov.pt/pt/apoio_contribuinte/Folhetos_informativos/Documents/Criptoativos.pdf)

9. [[PDF] MOD 3 IRS Anexo J - Portal das Finanças](https://info.portaldasfinancas.gov.pt/pt/apoio_contribuinte/modelos_formularios/irs/Documents/Mod_3_anexo_J.pdf)

10. [Page Image - Autoridade Tributária](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/irs/Pages/irs10.aspx)

11. [irs72ra_202512.aspx](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/cirs_rep/ra/Pages/irs72ra_202512.aspx)

12. [artigo 72.º do Código do IRS](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/cirs_rep/Pages/irs72.aspx)

13. [Program Rules - Breakout](https://www.breakoutprop.com/program-rules/)

14. [Maximum Daily Loss](https://www.breakoutprop.com/article/mastering-drawdown-a-guide-to-equity-limits-at-breakout/)

15. [HyroTrader Trading Rules for Crypto Prop Trading](https://www.hyrotrader.com/trading-rules/) - We support disciplined traders with structured rules that, when followed consistently, enable long-t...

16. [Crypto Prop Trading with Centralized Exchange Liquidity](https://www.breakoutprop.com/) - A crypto native prop firm trading on the best centralized exchange liquidity.

17. [Can I trade news?](https://ftmo.com/en/faq/can-i-trade-news/) - You may trade freely during all macroeconomic news releases, provided you do not engage in any Forbi...

18. [ftmo.com · en · futuresWhich instruments can I trade and what strategies am I ... - FTMO](https://ftmo.com/en/futures/faq/ftr-which-instruments-can-i-trade-and-what-strategies-am-i-allowed-to-use/) - With FTMO Futures, you can access a simulated trading environment and trade futures contracts (inclu...

19. [Forbidden Trading Practices | FTMO.com](https://ftmo.com/en/forbidden-trading-practices/) - Understand the FTMO prohibited practices to ensure a fair evaluation and secure your path to potenti...

20. [Order book | Hyperliquid Docs](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/order-book)

21. [Trading Update | 24 Jul 2025 | FTMO.com](https://ftmo.com/en/blog/trading-updates/trading-update-24-jul-2025/) - Weekend Maintenance, Revamp of Crypto Instruments Offering

22. [Crypto Prop Firm With No Time Limit on Your Evaluation](https://www.hyrotrader.com/prop-firm-with-no-time-limit/) - HyroTrader is a crypto prop firm with no time limit on evaluations. Trade at your own pace, get fund...

23. [How do I withdraw my reward?](https://ftmo.com/en/faq/how-do-i-withdraw-my-profits/) - Although FTMO Traders trade with simulated capital only, they are entitled to receive a Reward in th...

24. [How FTMO works | FTMO.com](https://ftmo.com/en/blog/how-ftmo-works/) - FTMO's mission is still the same from the beginning - to provide quality services to our clients, pu...

25. [www.hyrotrader.com › evaluationsCrypto Prop Trading Evaluation Options - HyroTrader](https://www.hyrotrader.com/evaluations/) - HyroTrader’s crypto challenge is designed to evaluate your trading skills and discipline through a Z...

26. [Crypto Prop Trading Strategies: 7 That Work in 2026](https://www.hyrotrader.com/blog/crypto-prop-trading-strategies/) - Seven crypto prop trading strategies for 24/7 markets: arbitrage, scalping, swing, market-making, an...

27. [One Step Evaluation Prop Firm for Crypto Traders](https://www.hyrotrader.com/one-step-evaluation-prop-firm/) - HyroTrader is a one step evaluation prop firm for crypto. Hit a 10% target with no time limit, get f...

28. [How to Become a Funded Trader: A Realistic 2026 Roadmap](https://www.hyrotrader.com/blog/how-to-become-a-funded-trader/) - How to become a funded trader: the realistic roadmap from challenge fee to scaling, the statistics, ...

29. [FAQ](https://cryptofundtrader.com/faq/) - Crypto Fund Trader is a simulated trading evaluation firm that gives traders the opportunity to prov...

30. [New updates FAQ](https://cryptofundtrader.com/last-updates-faq/) - an API Key must be generated and connected to the Crypto Fund Trader dashboard. Only USDT crypto fut...

31. [Crypto Fund Trader - ﻿ Best Trading Prop Firm | Up to $300.000](https://cryptofundtrader.com/) - Crypto Fund Trader is a prop firm with the best market conditions. No spread, low commission and mor...

32. [Terms and Conditions](https://cryptofundtrader.com/terms-and-conditions/) - During demo trading on the Trading Platform, you may carry out any transaction, unless they constitu...

33. [Choose Your Account Size](https://www.breakoutprop.com/kraken/)

34. [Breakout Evaluation Agreement - Breakout](https://www.breakoutprop.com/breakout-evaluation-agreement/) - Empowering successful traders

35. [How to start? - HyroTrader](https://www.hyrotrader.com/faq/getting-started/how-to-start/) - hyrotrader.com is a platform for trading crypto with Hyro

36. [Content-Dynamic](https://www.cmvm.pt/PInstitucional/ContentDynamic?Input=E12572A74A7F127A78395ED1ECFD76A72234216D175129F4D632CBB01AB93DD92413D96176F21D2045A44E2F6C8E7F98) - Consulte a lista de intermediários financeiros autorizados a prestar serviços em Portugal no Portal ...

37. [Entities authorised or registered with the CMVM ...](https://investidor.cmvm.pt/PInvestidor/Content?Input=01AE827E1A54D62F22C76E42BC0850B48F35632076F7A91B70B7C5C77F431C2C)

38. [Entidades autorizadas](https://www.cmvm.pt/PInvestidor/Content?Input=4942A283700EA992A5D112072B6A364B35C2A520C3180239B757BB7D8AF6295BEDA) - Lista de entidades autorizadas. Alertas aos investidores. Entidades não autorizadas - CMVMEntidades ...

39. [www.esma.europa.eu · sites · defaultFREQUENTLY ASKED QUESTIONS - European Securities and Markets...](https://www.esma.europa.eu/sites/default/files/library/esma71-98-125_faq_esmas_product_intervention_measures.pdf)

40. [ESMA agrees to prohibit binary options and restrict CFDs to ...](https://www.esma.europa.eu/press-news/esma-news/esma-agrees-prohibit-binary-options-and-restrict-cfds-protect-retail-investors)

41. [ESMA35-243228190-8024 Statement on identifying derivatives ...](https://www.esma.europa.eu/sites/default/files/2026-02/ESMA35-243228190-8024_-_Public_statement_on_derivatives_in_scope_of_the_CFD_product_intervention_measures.pdf)

42. [Banco de Portugal Notice no. 1/2023](https://www.bportugal.pt/sites/default/files/documents/2024-01/Aviso%20n.%C2%BA%201_2023_EN_PR_CQ_AS_TRD15232154475CB_2023-10-17_Banco%20de%20Portugal%2013122023.pdf)

43. [Aviso do Banco de Portugal n.o 2/2018](https://www.bportugal.pt/sites/default/files/anexos/cartas-circulares/329407737_5.docx.pdf)

44. [Funded Trader Agreement - Breakout](https://www.breakoutprop.com/funded-trader-agreement/) - Funded Trader shall perform its duties and otherwise conduct its trading activity in the Funded Acco...

45. [IRS > Faturas e Recibos > Fatura Recibo-verde - Portal das Finanças](https://info.portaldasfinancas.gov.pt/pt/apoio_contribuinte/questoes_frequentes/pages/faqs-00070.aspx)

46. [Artigo 115.º - Portal das Finanças](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/cirs_rep/Pages/irs115.aspx)

47. [Registo Contribuinte > Atividade > CAE/ Código CIRS](https://info.portaldasfinancas.gov.pt/pt/apoio_contribuinte/questoes_frequentes/Pages/faqs-00318.aspx)

48. [Portal das Finanças](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/irs/Pages/irs155.aspx)

49. [Ficha Doutrinária](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/informacoes_vinculativas/rendimento/cirs/Documents/PIV_16165.pdf)

50. [Ficha Doutrinária](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/informacoes_vinculativas/rendimento/cirs/Documents/PIV_14810.pdf)

51. [[PDF] 1. Os rendimentos auferidos por via de um contrato de prestação de ...](https://caad.org.pt/tributario/decisoes/view.php?l=MjAyMzA1MTkxNzI0MzYwLlA0MjNfMjAyMi1UIC0gMjAyMy0wNC0xMCAtIEpVUklTUFJVREVOQ0lBLnBkZg==)

52. [Perguntas Frequentes - sisscontent.seg-social.pt](https://sisscontent.seg-social.pt/documents/10152/14965/1009+Trabalhador+independente+-+novo+regime+-+Perguntas+Frequentes/9b8e7299-98c0-4d5b-9556-2e650604b72a)

53. [GUIA PRÁTICO - Segurança Social](https://www.seg-social.pt/storage1/files/1009---Novo-Regime-dos-Trabalhadores-Independentes-v1-09-8TvHiLaPC6okosI9oUn9_A.pdf)

54. [Taxas Contributivas](https://www.seg-social.pt/ptss/pssd/documento/cmd3e0ssc0019hn2y4z341qa8)

55. [IVA > Enquadramento Legal > Regras de Sujeição](https://info.portaldasfinancas.gov.pt/pt/apoio_contribuinte/questoes_frequentes/Pages/faqs-00924.aspx)

56. [rendimentos empresariais e profissionais – IRS e IVA](https://info.portaldasfinancas.gov.pt/pt/apoio_contribuinte/Folhetos_informativos/Documents/Inicio_atividade.pdf)

57. [Artigo 6.º](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/civa_rep/Pages/iva6.aspx)

58. [portal das finanças - Ficha Doutrinária](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/informacoes_vinculativas/rendimento/cirs/Documents/PIV_21506.pdf)

59. [Redação da Lei n.º 45-A/2024, de 31 de dezembro](https://info.portaldasfinancas.gov.pt/pt/informacao_fiscal/codigos_tributarios/CIRC_2R/Pages/irc87.aspx?print=true)

60. [O que é o IRC e quem tem de o pagar?](https://www.oe.gov.pt/financas-a-lupa/artigos/o-que-e-o-irc-e-quem-tem-de-o-pagar/) - Tal como as famílias pagam IRS sobre o seu rendimento, também as pessoas coletivas e outras entidade...

61. [Sabe quais são as principais novidades fiscais em 2026?](https://www.oe.gov.pt/noticias/sabe-quais-sao-as-principais-novidades-fiscais-em-2026/) - O Orçamento do Estado para 2026 reforçou as medidas do Governo para reduzir a carga fiscal sobre as ...

