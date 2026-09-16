//! French.
//!
//! Uses `vous`, not `tu`. This is a stranger filing a request with somebody
//! else's project, which is exactly the register `vous` is for — and `tu` would
//! read as presumptuous from software the reader has never used before.
//!
//! Typographic detail worth keeping: French puts a space before `?` and `:`, and
//! it should be a **non-breaking** one (`\u{a0}`) or the punctuation wraps onto
//! its own line at a narrow width — which is a phone, which is this surface's
//! premise.

use super::Strings;

pub static STRINGS: Strings = Strings {
    // A product name, not a word. Left as it is, deliberately.
    brand: "Smart Coder",
    theme_light: "Clair",
    theme_dark: "Sombre",
    language_label: "Langue",
    language_apply: "Appliquer",
    footer_tagline: " — déposez une demande, lisez la spécification qui en naît.",

    nav_signin: "Se connecter",
    nav_account: "Compte",
    nav_admin_heading: "Administration",
    nav_admin_review: "Demandes",
    nav_admin_settings: "Réglages",
    nav_admin_repos: "Dépôts",
    nav_admin_owners: "Responsables",
    nav_admin_daemons: "Machines",
    nav_admin_accounts: "Qui peut déposer",
    dialog_close: "Fermer",

    landing_headline: "Transformez chaque demande en spécification prête à développer.",
    landing_sub: "Tickets du support, demandes des commerciaux, idées lancées dans un couloir : tout arrive sous forme de prose vague. Smart Coder confronte chacune à votre code et rédige la spécification — qu'un de vos développeurs approuve, renvoie ou écarte.",
    landing_point_1_title: "Une spécification, pas une pull request",
    landing_point_1_body: "Il rédige la spécification et s'arrête là. Aucune branche, aucun correctif, rien de fusionné — vos développeurs écrivent toujours le code, à partir d'un document qui nomme déjà les fichiers et les cas limites.",
    landing_point_2_title: "Ancrée dans votre code",
    landing_point_2_body: "Chaque spécification est rédigée face au dépôt tel qu'il est, et non face à ce que la demande supposait. C'est toute la différence entre un ticket reformulé et un texte dont on peut partir.",
    landing_point_3_title: "Une personne garde la porte",
    landing_point_3_body: "Rien n'entre dans votre dépôt tant qu'un de vos développeurs n'a pas accepté. Les personnes que vous désignez peuvent renvoyer ou écarter une spécification ; seul le propriétaire peut accepter.",

    landing_eyebrow: "La réception des demandes, pour les équipes techniques",
    landing_cta: "Déposer une demande",
    landing_cta_signed_in: "Voir vos demandes",
    landing_cta_note: "Gratuit et open source. À héberger vous-même, dans un conteneur.",
    landing_oss_badge: "Gratuit et open source — licence MIT",

    landing_for_heading: "À qui cela s'adresse",
    landing_for_body: "Aux équipes techniques dont le backlog arrive de partout à la fois — support, commerciaux, clients, direction — et toujours sous la mauvaise forme. Vous déployez Smart Coder pour vos propres dépôts, décidez qui peut y déposer, et recevez des spécifications au lieu d'une file de prose que personne n'a le temps de trier.",
    landing_side_filers_title: "Pour celles et ceux qui demandent",
    landing_side_filers_body: "Aucun outil à apprendre, aucun formulaire à remplir. Ils décrivent le problème avec leurs mots, dans leur langue, et suivent ce qu'il en advient. À vous de décider si cela concerne vos équipes, vos clients, ou toute personne disposant du lien.",
    landing_side_team_title: "Pour l'équipe qui reçoit",
    landing_side_team_body: "Les demandes arrivent déjà instruites : ce qu'elles touchent, le comportement attendu, les endroits délicats. Le spam est filtré avant d'atteindre quiconque, et chaque compte est plafonné : un formulaire ouvert ne devient pas une passoire.",

    landing_how_heading: "Comment ça marche",
    landing_step_1_title: "Quelqu'un dépose une demande",
    landing_step_1_body: "Via un formulaire web que vous hébergez, en une ou deux phrases. La personne choisit le dépôt et s'il s'agit d'un bogue, d'une fonctionnalité ou d'une amélioration — rien de plus technique que cela.",
    landing_step_2_title: "Votre machine rédige la spécification",
    landing_step_2_body: "Un démon, sur votre propre matériel, récupère la demande, la confronte au dépôt qu'il détient, et rédige une spécification nommant les fichiers, le comportement et les cas limites.",
    landing_step_3_title: "Votre développeur tranche",
    landing_step_3_body: "La spécification revient pour relecture. Acceptez-la et elle est inscrite dans le dépôt sous forme de fichier Markdown, prête à développer. Renvoyez-la pour une nouvelle version, ou écartez-la.",

    landing_points_heading: "Ce qui fait la différence",

    landing_host_heading: "Votre code et votre modèle restent chez vous",
    landing_host_body: "Qui que ce soit qui héberge la boîte de réception, la machine qui lit votre dépôt vous appartient. Cette frontière est architecturale et non contractuelle : le serveur de réception n'a aucun moyen d'atteindre un dépôt.",
    landing_host_1_title: "Le serveur ne détient aucun code",
    landing_host_1_body: "Ni dépôt, ni système de fichiers, ni modèle — rien de tout cela n'y est lié. Des demandes d'un côté, des spécifications de l'autre : il ne détient jamais que du texte.",
    landing_host_2_title: "Rien n'écoute sur votre réseau",
    landing_host_2_body: "La machine qui rédige appelle vers l'extérieur et attend du travail. Aucun port entrant, aucune ouverture de pare-feu, aucun certificat — elle fonctionne derrière un NAT d'entreprise exactement comme ailleurs.",
    landing_host_3_title: "Votre modèle, votre matériel",
    landing_host_3_body: "Les spécifications sont rédigées là où se trouve déjà votre code, par un modèle que vous désignez. Faites tourner un petit modèle en local et votre code n'est jamais envoyé nulle part.",

    landing_oss_heading: "Gratuit, et ouvert à la lecture",
    landing_oss_body: "Sous licence MIT, et l'ensemble est sur GitHub — le serveur, le démon, et les spécifications qui l'ont fait naître. Ni sièges, ni clés de licence, aucune fonctionnalité réservée à une édition payante, et rien que vous ne puissiez lire avant de l'exécuter.",
    landing_oss_cta: "Lire le code source",

    landing_tiers_heading: "Deux façons de l'exécuter",
    landing_tiers_body: "Le logiciel est le même dans les deux cas. Ce qu'un abonnement achète n'est pas une fonctionnalité : c'est de ne pas avoir à héberger le serveur vous-même.",
    landing_tier_self_title: "Hébergez-le vous-même",
    landing_tier_self_tag: "Gratuit, dès aujourd'hui",
    landing_tier_self_body: "Un conteneur, un volume, un port, sur votre propre infrastructure. Toutes les fonctionnalités, sans limite, sans compte chez nous — et c'est vous qui le maintenez. C'est le produit entier, et cela le restera.",
    landing_tier_self_cta: "Lire le code source",
    landing_tier_hosted_title: "Nous l'hébergeons pour vous",
    landing_tier_hosted_tag: "Prévu — pas encore disponible",
    landing_tier_hosted_body: "Une boîte de réception hébergée, sur abonnement : aucun conteneur à déployer, aucune mise à jour à appliquer, aucun volume à sauvegarder. Pointez-y votre démon et continuez. Votre code et votre modèle ne quittent toujours pas vos machines.",
    landing_tier_hosted_cta: "Manifester votre intérêt",
    landing_tiers_note: "Dans les deux cas, le démon tourne sur votre matériel : la boîte de réception ne détient que le texte des demandes et les spécifications rédigées, jamais votre dépôt.",

    landing_demo_heading: "Cette page en est un exemplaire vivant",
    landing_demo_body: "Smart Coder reçoit ses propres demandes par le déploiement que vous êtes en train de lire. Déposez-en une sur le projet lui-même et regardez-la revenir en spécification — le chemin exact qu'emprunteraient vos propres interlocuteurs.",
    landing_demo_note: "Une démonstration du produit, et non un canal de support pour le vôtre.",
    landing_demo_cta: "Essayer sur ce dépôt",

    landing_close_heading: "Déployez-le pour votre équipe",
    landing_close_body: "Un conteneur, un volume, un port. Revendiquez-le, nommez vos dépôts, et pointez un démon vers le code que vous avez déjà.",

    signin_title: "Se connecter",
    signin_intro: "Déposer une demande nécessite une adresse e-mail — c'est ce qui vous \
                   permet de retrouver ce que vous avez déposé, et cela évite que ce \
                   formulaire ne soit ouvert à tous les vents.",
    signin_email_label: "E-mail",
    signin_email_placeholder: "vous@exemple.com",
    signin_submit: "Envoyez-moi un lien",
    signin_user_label: "E-mail",
    signin_password_label: "Mot de passe",
    signin_wrong: "Cela n'a pas fonctionné. Vérifiez le nom et le mot de passe, puis réessayez dans un instant.",
    signin_password_heading: "Connexion administrateur",
    signin_password_submit: "Se connecter",
    signin_register: "Créer un compte",
    signin_no_mail: "Ce site ne peut pas envoyer de liens de connexion pour le moment. Réessayez plus tard.",
    signin_no_password: "Pas de mot de passe. Nous envoyons un lien à usage unique, \
                         valable quinze minutes.",
    signin_no_password_note: "Pas de mot de passe. Nous envoyons un lien à usage unique, \
                              valable quinze minutes. Déposer une première demande crée \
                              le compte.",
    signin_sent: "Si cette adresse peut se connecter ici, un lien est en route. Il ne \
                  fonctionne qu'une fois, pendant quinze minutes.",

    sent_title: "Consultez votre boîte mail",
    sent_body: "Si cette adresse peut recevoir du courrier, un lien de connexion est en \
                route. Il expire dans quinze minutes.",
    sent_nothing_yet: "Rien d'autre ne s'est encore produit — c'est le lien qui vous connecte.",

    confirm_title: "Confirmer la connexion",
    confirm_intro: "Appuyez sur le bouton pour terminer la connexion sur cet appareil.",
    confirm_submit: "Connectez-moi",
    confirm_not_you: "Si vous n'êtes pas à l'origine de cette demande, fermez la page — \
                      rien ne se passe tant que vous n'appuyez pas.",

    link_failed_title: "Ce lien n'a pas fonctionné",
    link_already_used: "Ce lien a déjà été utilisé. Vous êtes probablement déjà connecté — ",
    link_already_used_link: "essayez de déposer une demande",
    link_expired: "Ce lien n'est plus valable. Ils expirent au bout de quinze minutes.",
    link_ask_again: "En demander un nouveau",

    file_title: "Déposer une demande",
    file_prompt: "Que faut-il faire\u{a0}?",
    file_placeholder: "Décrivez-le comme vous le feriez à un collègue.",
    file_submit: "Déposer",
    file_cap_before: "Jusqu'à ",
    file_cap_after: " mots. Plus c'est court, mieux c'est — une spécification est rédigée \
                     à partir de ce que vous écrivez, elle n'en est pas la copie.",
    file_spec_note: " Vous pourrez lire la spécification qui en résultera.",
    file_kind_label: "Type",
    file_repo_label: "Projet",
    owner_title: "Relecture",
    owner_nothing: "Rien n'a encore été déposé pour vos projets.",
    owner_note: "Vous pouvez renvoyer une spécification pour une nouvelle version, ou l'écarter. L'accepter revient au développeur.",
    owner_note_label: "Qu'est-ce qui ne va pas ?",
    owner_note_hint: "La prochaine version s'appuiera là-dessus.",
    owner_send_back: "Renvoyer",
    owner_discard: "Écarter",
    owner_release: "Libérer — ce n'est pas du spam",
    owner_release_note: "Le filtrage l'a retenu. Lisez-le et décidez ; le laisser ici ne décide rien.",
    file_no_repos: "Ce site ne reçoit pas de demandes pour le moment : aucun projet n'est ouvert. Réessayez plus tard.",
    file_repo_unknown: "Ce projet ne reçoit pas de demandes ici.",
    file_mine_heading: "Ce que vous avez déposé",
    file_nothing_yet: "Vous n'avez encore rien déposé.",
    file_signout: "Se déconnecter",

    filed_title: "Déposée",
    filed_body: "Déposée. Revenez sur cette page pour suivre ce qu'elle devient.",
    filed_feedback_body: "Merci — c'est enregistré. Les retours sont conservés pour que le \
                          développeur les lise\u{a0}; ils ne deviennent pas des spécifications.",
    back: "Retour",

    detail_asked_heading: "Ce que vous avez demandé",
    detail_spec_heading: "La spécification obtenue",
    detail_spec_withheld: "Une spécification a été rédigée et se trouve chez un relecteur.",
    detail_filed_prefix: "déposée ",

    state_received: "reçue",
    state_writing: "en cours de rédaction",
    state_reviewing: "chez un relecteur",
    state_accepted: "acceptée",
    state_closed: "close",

    // "il y a 5 min" — the marker goes in front in French, which is why these
    // are a prefix/suffix pair rather than a suffix.
    ago_just_now: "à l'instant",
    ago_prefix: "il y a ",
    ago_minutes: " min",
    ago_hours: " h",
    ago_days: " jours",

    error_empty: "Une demande a besoin d'un texte.",
    error_too_long: "C'est plus long que ce que ce formulaire accepte.",
    not_found_title: "Introuvable",
    not_found_body: "Il n'y a rien ici.",

    // -- l'interface --------------------------------------------------------
    //
    // Traduit plutôt que transposé mot à mot. L'anglais de cet outil est direct
    // et sans emphase ; le français vise le même registre — phrases courtes,
    // vocabulaire courant, pas de tournures administratives.
    nav_mine: "Vos demandes",
    nav_review: "Demandes à relire",
    nav_signout: "Se déconnecter",
    nav_requests: "Demandes",
    nav_overview: "Présentation",
    requests_anon_title: "Connectez-vous pour voir vos demandes",
    requests_anon_body: "Cette page montre ce que vous avez déposé et où cela en est. Se \
                         connecter demande une adresse e-mail, et rien d'autre.",
    theme_to_light: "Clair",
    theme_to_dark: "Sombre",
    footer_tagline_app: " — la réception des demandes qui rend une spécification.",

    filing_heading: "Que faut-il faire\u{a0}?",
    filing_text_label: "De quoi avez-vous besoin\u{a0}?",
    filing_text_placeholder: "Décrivez le changement avec vos propres mots.",
    filing_kind_label: "Est-ce cassé ou manquant\u{a0}?",
    kind_feature: "Il manque quelque chose",
    kind_bug: "Quelque chose est cassé",
    filing_repo_label: "Quel projet\u{a0}?",
    filing_submit: "Déposer",
    filing_done: "Déposée. Quelqu'un la rédige, et elle apparaît ci-dessous dès qu'il y a \
                  une spécification à lire.",
    filing_none_title: "Rien à quoi adresser une demande",
    filing_none_body: "Ce serveur ne propose aucun dépôt aux demandes pour le moment.",

    review_heading: "Demandes",
    review_empty_title: "Rien à relire",
    review_empty_body: "Dès qu'une demande est déposée puis rédigée, elle apparaît ici.",
    // "sonder" plutôt que "interroger" : c'est la machine qui vient chercher du
    // travail, pas le serveur qui la questionne.
    review_no_daemon: "Aucune machine n'a sondé ce serveur récemment, donc rien ne prendra \
                       cette demande en charge. Démarrez un démon.",
    review_unserved: "Des démons sondent le serveur, mais aucun ne propose ce dépôt. \
                      Vérifiez que le nom correspond à celui donné à queue add-repo.",
    review_skip_to_decision: "Aller directement à la décision",
    review_asked_heading: "Ce qui a été demandé",
    review_spec_heading: "La spécification rédigée",
    review_note_heading: "Note",
    review_landed_before: "Déposée dans ",
    // "À vous de décider" plutôt que "Votre appel" : "your call" est un idiome
    // que la traduction littérale rend incompréhensible.
    review_decide_heading: "À vous de décider",
    review_send_back_label: "La renvoyer — qu'est-ce qui doit changer\u{a0}?",
    review_send_back_placeholder: "La nouvelle version s'appuiera là-dessus, soyez précis.",
    review_send_back_submit: "Renvoyer",
    review_accept: "Approuver cette spécification",
    review_discard: "Écarter",
    review_leaving_decides_nothing: "Quitter cette page ne décide rien — elle sera toujours là.",
    review_quarantine_leaving: "La laisser ici ne décide rien.",

    // Les états bruts du protocole, rendus lisibles. Un relecteur tranche
    // justement sur la différence entre ces états ; un déposant voit la série
    // `state_*`, volontairement plus grossière.
    review_state_screening: "en filtrage",
    review_state_quarantined: "retenue",
    review_state_queued: "en file",
    review_state_claimed: "en cours de rédaction",
    review_state_awaiting_review: "en attente de relecture",
    review_state_accepted: "acceptée",
    review_state_discarded: "écartée",
    review_state_failed: "en échec",

    app_not_found_title: "Introuvable",
    app_not_found_body: "Il n'y a rien à cette adresse.",
    app_not_found_link: "Revenir au début",

    admin_saved: "Enregistré.",
    admin_save: "Enregistrer",
    admin_add: "Ajouter",
    admin_revoke: "Révoquer",
    admin_revoked_tag: "révoqué",

    settings_heading: "Réglages",
    settings_public_heading: "Le site public",
    settings_public_note: "Détermine si des personnes extérieures peuvent déposer des \
                           demandes ici. Sur un serveur fraîchement revendiqué, c'est \
                           désactivé.",
    settings_public_on: "Activer le site public",
    settings_public_off: "Désactiver le site public",
    settings_filers_heading: "Ce que voient les déposants",
    settings_show_spec: "Autoriser un déposant à lire la spécification rédigée à partir de \
                         sa propre demande",
    settings_stack_heading: "Défini dans la pile, pas ici",
    settings_stack_note: "L'adresse, le nom du site, les réglages de messagerie et le \
                          filtre anti-spam sont des variables d'environnement. Modifiez-les \
                          là où le conteneur est configuré, puis redéployez. Elles étaient \
                          autrefois modifiables ici et initialisées depuis la pile, si bien \
                          qu'une variable pouvait être définie, correcte, et pourtant \
                          ignorée sans un mot.",
    settings_ceilings_heading: "Plafonds",
    settings_ceilings_note: "Ce que ce serveur dépensera en une journée. Laisser vide \
                             applique la valeur par défaut, qui n'est pas zéro.",
    settings_max_filings: "Dépôts par jour",
    settings_max_drafts: "Rédactions par jour",
    settings_max_accounts: "Comptes",
    settings_max_links: "Liens de connexion en attente",

    owners_heading: "Responsables",
    owners_note: "Un responsable se connecte avec un identifiant et un mot de passe, et \
                  relit les demandes des dépôts que vous nommez ici. Il peut renvoyer un \
                  travail, le libérer et l'écarter — il ne peut pas l'accepter.",
    owners_add_heading: "Ajouter un responsable",
    owners_email_label: "Adresse e-mail",
    owners_repos_label: "Dépôts, séparés par des virgules",

    repos_heading: "Dépôts",
    repos_unserved_before: "Aucune machine ne propose ",
    repos_unserved_after: ". L'activer quand même signifie que les demandes déposées pour \
                           ce dépôt attendront qu'une machine le propose.",
    repos_enable_anyway: "L'activer quand même",
    repos_no_machine: "aucune machine ne le propose",
    repos_off_tag: "inactif",
    repos_turn_off: "Le désactiver",
    repos_add_heading: "Activer un dépôt",
    repos_name_label: "Nom du dépôt",
    repos_enable: "Activer",

    daemons_heading: "Machines",
    daemons_minted_after: " — cette clé n'est affichée qu'une fois et ne peut pas être \
                           retrouvée. Inscrivez-la dès maintenant dans la configuration de \
                           cette machine.",
    daemons_add_heading: "Ajouter une machine",
    daemons_label_label: "Un nom pour elle",
    daemons_mint: "Générer une clé",

    accounts_heading: "Qui peut déposer",
    accounts_note: "Les comptes révoqués restent listés. Une liste qui rétrécit sans un mot \
                    ne peut pas répondre à «\u{a0}est-ce que je m'en suis déjà \
                    occupé\u{a0}?\u{a0}».",
    accounts_password_tag: "mot de passe",

    setup_code_heading: "Configurer ce serveur",
    setup_code_intro: "Ce serveur n'a pas encore été revendiqué. Qui le revendique \
                       l'administre, c'est pourquoi le code ci-dessous est écrit dans le \
                       journal du conteneur\u{a0}: pouvoir lire ce journal est la preuve.",
    setup_code_label: "Le code de revendication, tiré du journal",
    setup_base_url_before: "Ce serveur répond à l'adresse ",
    setup_base_url_after: ", définie là où le conteneur est configuré.",
    setup_continue: "Continuer",
    setup_admin_heading: "Qui administre ce serveur\u{a0}?",
    setup_admin_intro: "Choisissez une adresse e-mail et un mot de passe. Ce compte \
                        administre ce serveur\u{a0}: il relit les demandes, décide ce que \
                        le site public collecte, et détient les clés. ",
    setup_admin_intro_strong: "Il n'y en a pas de second.",
    setup_email_label: "Adresse e-mail",
    setup_password_label: "Mot de passe",
    setup_min_password_before: "Au moins ",
    // Accord en nombre avec le compte qui précède, d'où la paire. Voir la
    // documentation du champ : `min_password` est configurable et peut valoir 1.
    setup_min_password_chars: " caractères",
    setup_min_password_chars_one: " caractère",
    setup_min_password_after: ". Il est stocké sous forme de condensat et ne peut pas être \
                               retrouvé — si vous le perdez, supprimez ",
    setup_min_password_tail: " du volume et revendiquez le serveur à nouveau.",
    setup_claim: "Le revendiquer",
};
