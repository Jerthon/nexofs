//! Chave de limitação e classes de operação. SPEC §7.2, §7.3.

use nexofs_domain::{AccountId, NamespaceId, ProviderId};
use std::time::Duration;

/// SPEC §7.3. Cada classe tem um limite de concorrência padrão (§7.8) e uma
/// prioridade padrão de despacho (§7.5) — ambos ajustáveis por configuração,
/// nunca zero (nenhuma classe pode ser suprimida por completo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationClass {
    InteractiveMetadata,
    InteractiveDownload,
    ChangeTracking,
    Upload,
    RemoteMutation,
    BackgroundIndex,
    Maintenance,
}

impl OperationClass {
    /// Limite inicial de concorrência por escopo (PRD §7.4 / SPEC §7.8).
    pub fn default_concurrency_limit(self) -> usize {
        match self {
            OperationClass::InteractiveMetadata => 2,
            OperationClass::InteractiveDownload => 4,
            OperationClass::ChangeTracking => 1,
            OperationClass::Upload => 2,
            OperationClass::RemoteMutation => 2,
            OperationClass::BackgroundIndex => 1,
            OperationClass::Maintenance => 1,
        }
    }

    /// Prioridade padrão de despacho (SPEC §7.5) — menor valor vence.
    /// Operações específicas (ex.: atualização manual vs. rastreamento de
    /// pasta ativa, ambas `ChangeTracking`) podem sobrepor este valor com
    /// uma `Priority` explícita ao enfileirar.
    pub fn default_priority(self) -> Priority {
        Priority(match self {
            OperationClass::InteractiveDownload => 10,
            OperationClass::InteractiveMetadata => 10,
            OperationClass::Upload => 20,
            OperationClass::ChangeTracking => 40,
            OperationClass::RemoteMutation => 50,
            OperationClass::BackgroundIndex => 80,
            OperationClass::Maintenance => 90,
        })
    }

    /// Teto de duração para uma chamada real ao provedor nesta classe —
    /// rede de segurança do Governor contra uma conexão que trava depois de
    /// estabelecida (sem RST/FIN, comum atrás de firewalls/proxies que
    /// descartam conexões ociosas silenciosamente): sem isso, a vaga de
    /// concorrência do escopo vazaria para sempre (bug real encontrado
    /// validando a Fase 2 contra o OneDrive: `in_flight` preso mesmo sem
    /// nenhum progresso de I/O, migrando entre `InteractiveMetadata` e
    /// `InteractiveDownload` conforme o indexador do sistema tentava
    /// acessar arquivos diferentes). Downloads/uploads têm teto maior por
    /// poderem legitimamente levar mais tempo (PRD §15.2, arquivos de até
    /// 100 GB); um timeout de inatividade por chunk (que não penalizaria
    /// transferências grandes mas lentas) é mais preciso e fica para o
    /// upload/download resumível da Fase 3 (T3-06).
    pub fn default_timeout(self) -> Duration {
        match self {
            OperationClass::InteractiveMetadata => Duration::from_secs(30),
            OperationClass::InteractiveDownload => Duration::from_secs(600),
            OperationClass::ChangeTracking => Duration::from_secs(60),
            OperationClass::Upload => Duration::from_secs(600),
            OperationClass::RemoteMutation => Duration::from_secs(30),
            OperationClass::BackgroundIndex => Duration::from_secs(60),
            OperationClass::Maintenance => Duration::from_secs(30),
        }
    }
}

/// Prioridade de despacho — menor valor é executado primeiro (SPEC §7.5).
/// O valor 0 é reservado para validações que evitam perda de dados.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(pub u8);

impl Priority {
    pub const DATA_LOSS_PREVENTION: Priority = Priority(0);
    pub const MANUAL_REFRESH: Priority = Priority(30);
    pub const PINNED_DOWNLOAD: Priority = Priority(60);
}

/// Escopo de limitação (SPEC §7.2). Duas requisições com o mesmo `RateScope`
/// competem pelo mesmo orçamento de concorrência; escopos diferentes nunca
/// se bloqueiam mutuamente — isso é o que impede uma indexação de baixo
/// nível monopolizar a capacidade necessária para abrir um arquivo.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateScope {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub organization_scope: Option<String>,
    pub namespace_id: Option<NamespaceId>,
    pub operation_class: OperationClass,
}
