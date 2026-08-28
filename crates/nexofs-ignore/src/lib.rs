//! Avaliação de exclusões (`.nexofsignore`). SPEC §17.
//!
//! Puro: não conhece SQLite nem o sistema de arquivos real (SPEC §3.2) —
//! recebe uma lista de regras já resolvidas (uma por camada de precedência)
//! e devolve a disposição vencedora junto com a regra que decidiu (T4-03).
//! Descobrir QUAIS regras se aplicam a um namespace/pasta (linhas do banco,
//! arquivos `.nexofsignore` encontrados subindo a árvore) é responsabilidade
//! de `nexofs-sync-core`, que tem acesso ao índice e ao FS reais.
//!
//! A sintaxe de cada padrão é compatível com `.gitignore` (SPEC §17.1) — a
//! análise de glob/negação/âncora/`**` é delegada à crate `ignore` (a mesma
//! usada pelo ripgrep), em vez de reimplementada aqui.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use nexofs_domain::states::SyncDisposition;
use std::path::Path;

/// SPEC §17.2 — da menor para a maior prioridade. A ordem de declaração
/// desta enum É a ordem de precedência: `derive(Ord)` compara por posição
/// de declaração, então `RuleTier::Defaults < RuleTier::UserException`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuleTier {
    Defaults,
    AdminPolicy,
    TechProfile,
    UserGlobal,
    Account,
    Folder,
    NexofsIgnoreFile,
    UserException,
}

/// Uma regra já resolvida para avaliação. `pattern` é uma linha completa em
/// sintaxe `.gitignore` (`!` de negação e `/` final de diretório fazem parte
/// do próprio texto, não campos separados — mesma convenção de um arquivo
/// `.nexofsignore` real). Várias regras da mesma camada (ex.: múltiplos
/// arquivos `.nexofsignore` na árvore) DEVEM ser passadas em ordem crescente
/// de precedência dentro da camada — a mais próxima/específica por último,
/// exatamente como um walker gitignore real cascateia.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub tier: RuleTier,
    pub pattern: String,
}

impl Rule {
    pub fn new(tier: RuleTier, pattern: impl Into<String>) -> Self {
        Self { tier, pattern: pattern.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// `NormalSync` quando incluído, `LocalOnly` quando excluído — os
    /// outros valores de `SyncDisposition` (`RemotePlaceholder`,
    /// `IgnoreChanges`) não são produzidos por regras de exclusão; vêm de
    /// outros mecanismos (ex.: fixação `OnlineOnly`, T4-10).
    pub disposition: SyncDisposition,
    /// `None` quando nenhuma regra de nenhuma camada se aplicou ao caminho
    /// — padrão de `.gitignore`: não listado significa incluído.
    pub winning_rule: Option<Rule>,
}

#[derive(Debug, thiserror::Error)]
#[error("padrão de exclusão inválido \"{pattern}\": {source}")]
pub struct RuleError {
    pub pattern: String,
    #[source]
    pub source: ignore::Error,
}

/// Motor de avaliação compilado para um conjunto fixo de regras. Compilar é
/// mais caro que avaliar — construa uma vez por namespace/pasta e reutilize
/// para cada caminho.
pub struct IgnoreEngine {
    // Um `Gitignore` por camada não vazia, já em ordem crescente de
    // precedência (`RuleTier` implementa `Ord` na ordem certa). Camadas sem
    // nenhuma regra aplicável simplesmente não entram aqui.
    tiers: Vec<(RuleTier, Gitignore, Vec<String>)>,
}

impl IgnoreEngine {
    /// `rules` não precisa estar pré-ordenada por camada — `build` agrupa e
    /// ordena internamente. A ordem RELATIVA de regras da MESMA camada é
    /// preservada (necessário para "`.nexofsignore` mais próximo vence").
    pub fn build(rules: &[Rule]) -> Result<Self, RuleError> {
        let mut by_tier: Vec<(RuleTier, Vec<&str>)> = Vec::new();
        for rule in rules {
            match by_tier.iter_mut().find(|(tier, _)| *tier == rule.tier) {
                Some((_, patterns)) => patterns.push(&rule.pattern),
                None => by_tier.push((rule.tier, vec![&rule.pattern])),
            }
        }
        by_tier.sort_by_key(|(tier, _)| *tier);

        let mut tiers = Vec::with_capacity(by_tier.len());
        for (tier, patterns) in by_tier {
            let mut builder = GitignoreBuilder::new("/");
            for pattern in &patterns {
                builder.add_line(None, pattern).map_err(|source| RuleError { pattern: pattern.to_string(), source })?;
            }
            let compiled = builder.build().map_err(|source| RuleError { pattern: patterns.join(", "), source })?;
            tiers.push((tier, compiled, patterns.into_iter().map(str::to_string).collect()));
        }

        Ok(Self { tiers })
    }

    /// Avalia `path` (relativo à raiz do namespace) contra todas as camadas,
    /// da menor para a maior prioridade — a última camada cuja regra
    /// realmente casar decide o resultado final (SPEC §17.2: "a última
    /// regra aplicável vence"), incluindo o caso de uma exceção explícita
    /// (`!padrão`) reincluir algo que uma camada de menor prioridade excluiu.
    ///
    /// Usa `matched_path_or_any_parents` (não `matched`) em cada camada: uma
    /// regra de diretório (`node_modules/`) precisa excluir tudo por baixo
    /// dele (`node_modules/pacote/index.js`), não só o próprio diretório —
    /// exatamente o caso de uso que motiva esta fase inteira (projetos
    /// Node/Laravel).
    pub fn evaluate(&self, path: &Path, is_dir: bool) -> Decision {
        let mut winner: Option<(RuleTier, &Gitignore, &[String])> = None;

        for (tier, gitignore, patterns) in &self.tiers {
            if !matches!(gitignore.matched_path_or_any_parents(path, is_dir), ignore::Match::None) {
                winner = Some((*tier, gitignore, patterns));
            }
        }

        let Some((tier, gitignore, patterns)) = winner else {
            return Decision {
                disposition: SyncDisposition::NormalSync,
                winning_rule: None,
            };
        };

        match gitignore.matched_path_or_any_parents(path, is_dir) {
            ignore::Match::Ignore(glob) => Decision {
                disposition: SyncDisposition::LocalOnly,
                winning_rule: Some(Rule::new(tier, resolve_original(glob, patterns))),
            },
            ignore::Match::Whitelist(glob) => Decision {
                disposition: SyncDisposition::NormalSync,
                winning_rule: Some(Rule::new(tier, resolve_original(glob, patterns))),
            },
            ignore::Match::None => unreachable!("`winner` só é preenchido quando o match não é None"),
        }
    }
}

fn resolve_original(glob: &ignore::gitignore::Glob, fallback_patterns: &[String]) -> String {
    let original = glob.original();
    if !original.is_empty() {
        return original.to_string();
    }
    fallback_patterns.first().cloned().unwrap_or_default()
}

/// T4-04/SPEC §17.4 — perfis de tecnologia sugeríveis a partir de um
/// arquivo-manifesto encontrado na raiz do projeto. Puramente dados: NUNCA
/// aplicado automaticamente (SPEC §17.4 "perfis sugeridos NÃO DEVEM ser
/// ativados silenciosamente") — cabe a quem chama decidir quando oferecer a
/// sugestão e exigir confirmação explícita antes de transformar `patterns`
/// em regras de verdade (`RuleTier::TechProfile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    pub manifest_file: &'static str,
    pub patterns: &'static [&'static str],
}

/// Detecção por nome exato de manifesto — cobre 5 dos 6 ecossistemas de
/// SPEC §17.4 (.NET não tem um nome de manifesto fixo — `*.csproj` varia
/// por projeto — e exigiria correspondência por glob em vez de nome exato;
/// fica para quando houver necessidade real de detectá-lo automaticamente).
pub const KNOWN_PROFILES: &[Profile] = &[
    Profile {
        name: "nodejs",
        manifest_file: "package.json",
        patterns: &["node_modules/", ".next/cache/", ".npm/", ".yarn/cache/", ".pnpm-store/"],
    },
    Profile {
        name: "php_laravel",
        manifest_file: "composer.json",
        patterns: &["vendor/", "storage/framework/cache/", "storage/framework/sessions/", "storage/framework/views/", "bootstrap/cache/"],
    },
    Profile {
        name: "python",
        manifest_file: "requirements.txt",
        patterns: &[".venv/", "venv/", "__pycache__/", ".pytest_cache/", ".mypy_cache/"],
    },
    Profile {
        name: "python",
        manifest_file: "pyproject.toml",
        patterns: &[".venv/", "venv/", "__pycache__/", ".pytest_cache/", ".mypy_cache/"],
    },
    Profile {
        name: "java_gradle",
        manifest_file: "build.gradle",
        patterns: &["target/", ".gradle/", "build/"],
    },
    Profile {
        name: "rust",
        manifest_file: "Cargo.toml",
        patterns: &["target/"],
    },
];

/// Analisa o conteúdo de um arquivo `.nexofsignore` (SPEC §17.1) e devolve
/// as linhas de padrão prontas para virar `Rule`s de `RuleTier::NexofsIgnoreFile`
/// — descarta comentários (`#`) e linhas em branco; preserva `!`/`/`/`**`
/// como parte do padrão, sem qualquer transformação (a crate `ignore`
/// interpreta a sintaxe na hora de compilar).
pub fn parse_nexofsignore(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.trim_start().starts_with('#'))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(rules: &[Rule], path: &str, is_dir: bool) -> Decision {
        IgnoreEngine::build(rules).unwrap().evaluate(Path::new(path), is_dir)
    }

    #[test]
    fn a_path_with_no_matching_rule_is_included_by_default() {
        let decision = decide(&[Rule::new(RuleTier::TechProfile, "node_modules/")], "src/main.rs", false);
        assert_eq!(decision.disposition, SyncDisposition::NormalSync);
        assert!(decision.winning_rule.is_none());
    }

    #[test]
    fn a_directory_only_pattern_matches_only_directories() {
        let rules = [Rule::new(RuleTier::TechProfile, "node_modules/")];
        assert_eq!(decide(&rules, "node_modules", true).disposition, SyncDisposition::LocalOnly);
        assert_eq!(decide(&rules, "node_modules", false).disposition, SyncDisposition::NormalSync, "arquivo comum chamado node_modules não é a pasta");
    }

    #[test]
    fn recursive_glob_matches_at_any_depth() {
        let rules = [Rule::new(RuleTier::TechProfile, "**/__pycache__/")];
        assert_eq!(decide(&rules, "__pycache__", true).disposition, SyncDisposition::LocalOnly);
        assert_eq!(decide(&rules, "a/b/c/__pycache__", true).disposition, SyncDisposition::LocalOnly);
    }

    #[test]
    fn a_higher_tier_rule_overrides_a_lower_tier_rule_for_the_same_path() {
        // SPEC §17.2: exceção explícita do usuário (camada 8) supera o
        // perfil de tecnologia (camada 3).
        let rules = [
            Rule::new(RuleTier::TechProfile, "vendor/"),
            Rule::new(RuleTier::UserException, "!vendor/important-fork/"),
        ];
        assert_eq!(decide(&rules, "vendor", true).disposition, SyncDisposition::LocalOnly);
        assert_eq!(
            decide(&rules, "vendor/important-fork", true).disposition,
            SyncDisposition::NormalSync,
            "exceção explícita do usuário deve vencer o perfil de tecnologia"
        );
    }

    #[test]
    fn a_lower_tier_cannot_override_a_higher_tier_even_when_declared_later() {
        // Ordem de construção não importa — só a camada. Uma regra de conta
        // (camada 5) não pode desfazer uma exceção de usuário (camada 8)
        // mesmo que a de conta seja "mais nova" na lista de entrada.
        let rules = [
            Rule::new(RuleTier::UserException, "!segredo.txt"),
            Rule::new(RuleTier::Account, "segredo.txt"),
        ];
        assert_eq!(decide(&rules, "segredo.txt", false).disposition, SyncDisposition::NormalSync);
    }

    #[test]
    fn the_closest_nexofsignore_file_wins_within_its_own_tier() {
        // Duas regras na MESMA camada (dois arquivos `.nexofsignore` em
        // pontos diferentes da árvore) — a última da lista (mais próxima do
        // arquivo, por convenção de quem monta a lista) vence dentro da
        // camada, sem precisar de nenhuma outra camada mais alta.
        let rules = [
            Rule::new(RuleTier::NexofsIgnoreFile, "*.log"),
            Rule::new(RuleTier::NexofsIgnoreFile, "!important.log"),
        ];
        assert_eq!(decide(&rules, "debug.log", false).disposition, SyncDisposition::LocalOnly);
        assert_eq!(decide(&rules, "important.log", false).disposition, SyncDisposition::NormalSync);
    }

    #[test]
    fn winning_rule_reports_the_tier_and_pattern_that_decided() {
        let rules = [Rule::new(RuleTier::TechProfile, "target/")];
        let decision = decide(&rules, "target", true);
        let winner = decision.winning_rule.unwrap();
        assert_eq!(winner.tier, RuleTier::TechProfile);
        assert_eq!(winner.pattern, "target/");
    }

    #[test]
    fn parses_nexofsignore_content_ignoring_comments_and_blank_lines() {
        let content = "# comentário\n\nnode_modules/\n!node_modules/keep-me/\n  # outro comentário indentado\n*.tmp\n";
        let parsed = parse_nexofsignore(content);
        assert_eq!(parsed, vec!["node_modules/", "!node_modules/keep-me/", "*.tmp"]);
    }

    #[test]
    fn a_file_deep_inside_an_excluded_directory_is_excluded_too() {
        // O caso de uso central desta fase (Node/Laravel): uma regra sobre a
        // pasta precisa valer para tudo por baixo dela, não só para a
        // própria entrada de diretório.
        let rules = [Rule::new(RuleTier::TechProfile, "node_modules/")];
        let decision = decide(&rules, "node_modules/deep/file.js", false);
        assert_eq!(decision.disposition, SyncDisposition::LocalOnly);
    }

    #[test]
    fn an_exception_deep_inside_an_excluded_directory_can_still_reinclude_it() {
        let rules = [
            Rule::new(RuleTier::TechProfile, "vendor/"),
            Rule::new(RuleTier::UserException, "!vendor/meu-pacote/**"),
        ];
        assert_eq!(decide(&rules, "vendor/outro-pacote/index.php", false).disposition, SyncDisposition::LocalOnly);
        assert_eq!(decide(&rules, "vendor/meu-pacote/index.php", false).disposition, SyncDisposition::NormalSync);
    }
}
