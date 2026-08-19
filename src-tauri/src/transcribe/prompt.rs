//! Whisper initial-prompt construction.
//!
//! Port of `Transcriber._build_prompt` / `TECH_PROMPT` from
//! `src/vibetotext/transcriber.py` (and the C# `WhisperTranscriber.TechPrompt`).
//! The prompt biases Whisper toward programming/technical vocabulary, and an
//! optional custom-dictionary section instructs exact spelling for user terms.

/// Technical vocabulary prompt biasing Whisper toward programming terms.
///
/// Copied verbatim from `transcriber.py`'s `TECH_PROMPT` so transcription
/// quality matches the reference implementations exactly.
pub const TECH_PROMPT: &str =
    "This is a software engineer dictating code and technical documentation.\n\
They frequently discuss: APIs, databases, frontend frameworks, backend services,\n\
cloud infrastructure, and AI/ML systems. Use programming terminology and proper\n\
capitalization for technical terms.\n\
\n\
Common terms: Firebase, Firestore, MongoDB, PostgreSQL, MySQL, Redis, SQLite,\n\
API, REST, GraphQL, gRPC, WebSocket, JSON, YAML, XML, HTML, CSS, SCSS,\n\
JavaScript, TypeScript, Python, Rust, Go, Java, C++, Swift, Kotlin,\n\
React, Vue, Angular, Svelte, Next.js, Nuxt, Remix, Astro,\n\
Node.js, Deno, Bun, npm, yarn, pnpm, webpack, Vite, esbuild, Rollup,\n\
Docker, Kubernetes, K8s, Helm, Terraform, Ansible, Jenkins, CircleCI,\n\
AWS, S3, EC2, Lambda, DynamoDB, CloudFront, Route53, ECS, EKS,\n\
GCP, BigQuery, Cloud Run, Cloud Functions, Pub/Sub,\n\
Azure, Vercel, Netlify, Railway, Render, Fly.io, Cloudflare,\n\
Git, GitHub, GitLab, Bitbucket, PR, pull request, merge, rebase, cherry-pick,\n\
CI/CD, DevOps, SRE, microservices, monorepo, serverless, edge functions,\n\
useState, useEffect, useContext, useRef, useMemo, useCallback, useReducer,\n\
Redux, Zustand, Jotai, Recoil, MobX, XState,\n\
Prisma, Drizzle, TypeORM, Sequelize, Knex, SQLAlchemy,\n\
tRPC, Zod, Yup, Joi, Express, Fastify, Hono, FastAPI, Flask, Django,\n\
Tailwind, styled-components, Emotion, CSS Modules, Sass,\n\
Jest, Vitest, Cypress, Playwright, Testing Library,\n\
ESLint, Prettier, Biome, TypeScript, TSConfig,\n\
OAuth, JWT, session, cookie, CORS, CSRF, XSS, SQL injection,\n\
Claude, Anthropic, OpenAI, GPT, Gemini, Llama, Mistral,\n\
LLM, embedding, vector database, Pinecone, Weaviate, ChromaDB, Qdrant,\n\
RAG, retrieval, chunking, tokenization, fine-tuning, RLHF, prompt engineering,\n\
Whisper, transcription, TTS, speech-to-text, ASR, NLP, NLU,\n\
regex, cron, UUID, Base64, SHA, MD5, RSA, AES, TLS, SSL, HTTPS.";

/// Build the full Whisper initial prompt, appending a custom-dictionary section
/// when `custom_words` is non-empty.
///
/// Mirrors `Transcriber._build_prompt`: the base [`TECH_PROMPT`] alone when there
/// is no dictionary, otherwise the prompt plus an exact-spelling instruction
/// listing the user's terms twice (once to introduce them, once to reinforce the
/// exact spelling) — matching the Python/C# wording verbatim.
pub fn build_prompt(custom_words: &[String]) -> String {
    if custom_words.is_empty() {
        return TECH_PROMPT.to_string();
    }

    let words_list = custom_words.join(", ");
    format!(
        "{TECH_PROMPT}\n\nIMPORTANT: The speaker uses these specific terms that must be \
         transcribed exactly as spelled: {words_list}. When you hear anything similar to \
         these words, use the exact spelling provided: {words_list}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dictionary_returns_base_prompt() {
        let prompt = build_prompt(&[]);
        assert_eq!(prompt, TECH_PROMPT);
        // The base prompt must carry the programming-vocabulary bias.
        assert!(prompt.contains("software engineer"));
        assert!(prompt.contains("PostgreSQL"));
        assert!(!prompt.contains("exactly as spelled"));
    }

    #[test]
    fn custom_words_appended_with_exact_spelling_instruction() {
        let words = vec!["Kubernetes".to_string(), "Zustand".to_string()];
        let prompt = build_prompt(&words);

        // Base prompt is preserved.
        assert!(prompt.starts_with(TECH_PROMPT));
        // Exact-spelling instruction is appended.
        assert!(prompt.contains("transcribed exactly as spelled"));
        // The custom words are listed (twice, per the reference wording).
        assert_eq!(prompt.matches("Kubernetes, Zustand").count(), 2);
    }

    #[test]
    fn single_custom_word_has_no_trailing_comma() {
        let words = vec!["VibeToText".to_string()];
        let prompt = build_prompt(&words);
        assert!(prompt.contains("exactly as spelled: VibeToText."));
    }
}
